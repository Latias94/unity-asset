use std::fmt;
use std::io::{self, Write};
use std::mem::size_of;
use std::sync::{Arc, RwLock, RwLockWriteGuard};

use unity_asset::{
    AssetLoadBudget, ChangeSet, DigestV1, SourceId, TransactionId, WorkspaceId, WorkspaceRevision,
};
use unity_asset_core::{string_allocation_bytes, vec_allocation_bytes};
use unity_asset_search_core::SearchRequest;
use unity_asset_search_protocol::{
    GenerationMaintenanceState, GenerationMaintenanceStatus, MAX_REINDEX_PUBLISH_WARNINGS,
    ReferenceRequest, ReferencesResponse, ReindexEvidence, SearchResponse, SuggestResponse,
};

use crate::analysis::{AnalysisMetrics, AssetAnalysis, AssetAnalysisBatch};
use crate::config::{IndexPaths, SearchIndexOptions};
use crate::generation::{
    GenerationProjectionDigests, GenerationProjectionSummary, GenerationStamp,
    SearchGenerationIdentityV1, SearchGenerationManifestV1,
};
#[cfg(test)]
use crate::generation_store::GenerationFailpoint;
use crate::generation_store::{
    DesiredRevisionCommit, GenerationActivationEvidence, GenerationBuild, GenerationDiskEstimate,
    GenerationPublishWarning, GenerationPublishWarningKind, GenerationRebuildBootstrap,
    GenerationSnapshot, GenerationStore, GenerationStoreError, GenerationStoreOptions,
    SourceScanHint, SourceStateSnapshot, TransactionReceiptMembership, TransactionReceiptWindow,
};
use crate::path_semantics::compare_portable_paths;
use crate::pipeline::PipelineError;
use crate::projection::{
    GenerationProjection, ProjectionCategory, ProjectionError, ProjectionLimits, ProjectionMetrics,
    project_batch,
};
use crate::query::{QueryEngine, QuerySnapshot, SearchQueryFields};
use crate::reference_query::{
    ReferenceQueryCompleteness, ReferenceQueryCompletenessError, ReferenceQueryEngine,
    ReferenceQueryError, ReferenceQuerySnapshot,
};
use crate::scan::{ProjectScanner, ProjectSourcePath, ScanValidation};
use crate::semantics::{AnalysisCacheIdentityV1, SearchSemantics};
use crate::source_coordinate::IndexedSourceCoordinate;
use crate::store::{ProjectionReaders, ProjectionStore, is_rebuildable_projection_schema_version};

#[derive(Clone)]
pub(crate) struct ActiveGeneration {
    pub(crate) snapshot: GenerationSnapshot,
    pub(crate) stamp: GenerationStamp,
    query: QueryEngine,
    references: ReferenceQueryEngine,
    indexed_assets: u64,
    indexed_search_documents: u64,
    indexed_reference_facts: u64,
    incomplete_assets: u64,
    projection_truncations: u64,
}

impl fmt::Debug for ActiveGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveGeneration")
            .field("stamp", &self.stamp)
            .field("directory", &self.snapshot.directory())
            .field("indexed_assets", &self.indexed_assets)
            .field("indexed_search_documents", &self.indexed_search_documents)
            .field("indexed_reference_facts", &self.indexed_reference_facts)
            .field("incomplete_assets", &self.incomplete_assets)
            .field("projection_truncations", &self.projection_truncations)
            .finish_non_exhaustive()
    }
}

impl ActiveGeneration {
    fn open(
        snapshot: GenerationSnapshot,
        source_state: &SourceStateSnapshot,
        readers: &ProjectionReaders,
        projection: Option<&GenerationProjection>,
        options: SearchIndexOptions,
        semantics_current: bool,
        configuration_current: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PipelineError> {
        let manifest = snapshot.manifest();
        let generation_current = semantics_current && configuration_current;
        let stamp = GenerationStamp::current(
            snapshot.generation(),
            manifest.workspace(),
            manifest.revision(),
        )
        .with_desired_revision(snapshot.desired_revision())
        .with_semantics_current(semantics_current)
        .with_configuration_current(configuration_current);
        let search_fields = SearchQueryFields::from_schema(&readers.search().index().schema())
            .map_err(PipelineError::Query)?;
        let query_snapshot = match projection {
            Some(projection) => QuerySnapshot::new(
                stamp.clone(),
                readers.search().reader().clone(),
                search_fields.clone(),
                projection
                    .search_documents
                    .iter()
                    .map(|document| document.path.as_str()),
                budget,
            ),
            None if generation_current => QuerySnapshot::new(
                stamp.clone(),
                readers.search().reader().clone(),
                search_fields.clone(),
                suggestion_paths_from_state(source_state, options),
                budget,
            ),
            None => {
                let paths = readers
                    .search()
                    .stored_paths(budget)
                    .map_err(PipelineError::Projection)?;
                QuerySnapshot::new(
                    stamp.clone(),
                    readers.search().reader().clone(),
                    search_fields,
                    paths,
                    budget,
                )
            }
        };
        let query = QueryEngine::new(Arc::new(query_snapshot.map_err(PipelineError::Query)?));

        let analysis_complete = source_state
            .assets()
            .iter()
            .all(|analysis| analysis.complete);
        let projection_complete = projection.map_or_else(
            || generation_current && projection_is_complete(source_state, options),
            |projection| {
                projection.truncations.iter().all(|truncation| {
                    !matches!(&truncation.category, ProjectionCategory::References)
                })
            },
        );
        let completeness = match projection {
            Some(projection) => ReferenceQueryCompleteness::new(
                analysis_complete,
                projection_complete,
                projection.diagnostics.iter(),
                budget,
            ),
            None => ReferenceQueryCompleteness::new(
                analysis_complete,
                projection_complete,
                source_state
                    .assets()
                    .iter()
                    .flat_map(|analysis| analysis.diagnostics.iter()),
                budget,
            ),
        }
        .map_err(|error| match error {
            ReferenceQueryCompletenessError::Budget(error) => PipelineError::Budget(error),
            error => PipelineError::Query(error.into()),
        })?;
        let reference_snapshot =
            ReferenceQuerySnapshot::new(stamp.clone(), readers.references(), completeness);
        let references = ReferenceQueryEngine::new(Arc::new(reference_snapshot));
        let summary = snapshot.manifest().projection_summary();
        validate_projection_summary(summary, source_state, readers)?;

        Ok(Self {
            snapshot,
            stamp,
            query,
            references,
            indexed_assets: summary.assets(),
            indexed_search_documents: summary.search_documents(),
            indexed_reference_facts: summary.reference_documents(),
            incomplete_assets: summary.incomplete_assets(),
            projection_truncations: summary.projection_truncations(),
        })
    }

    fn open_projection_only(
        snapshot: GenerationSnapshot,
        readers: &ProjectionReaders,
        semantics_current: bool,
        configuration_current: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PipelineError> {
        let manifest = snapshot.manifest();
        let stamp = GenerationStamp::current(
            snapshot.generation(),
            manifest.workspace(),
            manifest.revision(),
        )
        .with_desired_revision(snapshot.desired_revision())
        .with_semantics_current(semantics_current)
        .with_configuration_current(configuration_current);
        let search_fields = SearchQueryFields::from_schema(&readers.search().index().schema())
            .map_err(PipelineError::Query)?;
        let paths = readers
            .search()
            .stored_paths(budget)
            .map_err(PipelineError::Projection)?;
        let query_snapshot = QuerySnapshot::new(
            stamp.clone(),
            readers.search().reader().clone(),
            search_fields,
            paths,
            budget,
        )
        .map_err(PipelineError::Query)?;
        let completeness =
            ReferenceQueryCompleteness::new(false, false, std::iter::empty(), budget).map_err(
                |error| match error {
                    ReferenceQueryCompletenessError::Budget(error) => PipelineError::Budget(error),
                    error => PipelineError::Query(error.into()),
                },
            )?;
        let references =
            ReferenceQuerySnapshot::new(stamp.clone(), readers.references(), completeness);
        validate_projection_summary_without_source_state(manifest.projection_summary(), readers)?;
        let summary = manifest.projection_summary();
        Ok(Self {
            snapshot,
            stamp,
            query: QueryEngine::new(Arc::new(query_snapshot)),
            references: ReferenceQueryEngine::new(Arc::new(references)),
            indexed_assets: summary.assets(),
            indexed_search_documents: summary.search_documents(),
            indexed_reference_facts: summary.reference_documents(),
            incomplete_assets: summary.incomplete_assets(),
            projection_truncations: summary.projection_truncations(),
        })
    }

    pub(crate) const fn stamp(&self) -> &GenerationStamp {
        &self.stamp
    }

    pub(crate) const fn indexed_assets(&self) -> u64 {
        self.indexed_assets
    }

    pub(crate) const fn indexed_search_documents(&self) -> u64 {
        self.indexed_search_documents
    }

    pub(crate) const fn indexed_reference_facts(&self) -> u64 {
        self.indexed_reference_facts
    }

    pub(crate) const fn incomplete_assets(&self) -> u64 {
        self.incomplete_assets
    }

    pub(crate) const fn projection_truncations(&self) -> u64 {
        self.projection_truncations
    }

    pub(crate) fn search(&self, request: SearchRequest) -> anyhow::Result<SearchResponse> {
        let mut response = self.query.search(request)?;
        response.generation = crate::wire::generation_stamp(&self.stamp);
        Ok(response)
    }

    pub(crate) fn suggest(&self, prefix: &str, limit: usize) -> anyhow::Result<SuggestResponse> {
        let mut response = self.query.suggest(prefix, limit)?;
        response.generation = crate::wire::generation_stamp(&self.stamp);
        Ok(response)
    }

    pub(crate) fn references(
        &self,
        request: ReferenceRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferencesResponse, ReferenceQueryError> {
        let mut response = self.references.references(request, budget)?;
        response.generation = crate::wire::generation_stamp(&self.stamp);
        Ok(response)
    }

    fn install_committed_snapshot(
        &mut self,
        snapshot: GenerationSnapshot,
    ) -> Result<(), PipelineError> {
        if snapshot.generation() != self.snapshot.generation()
            || snapshot.storage_contract() != self.snapshot.storage_contract()
            || snapshot.manifest() != self.snapshot.manifest()
            || snapshot.directory() != self.snapshot.directory()
            || snapshot.activation_ordinal() < self.snapshot.activation_ordinal()
        {
            return Err(PipelineError::Invariant(
                "desired-revision commit changed immutable active-generation identity",
            ));
        }
        self.stamp = self
            .stamp
            .clone()
            .with_desired_revision(snapshot.desired_revision());
        self.snapshot = snapshot;
        Ok(())
    }
}

/// Shared in-process authority for the immutable generation used by queries and status.
///
/// The write guard is held across durable activation and the corresponding in-memory install, so
/// readers cannot observe a disk-committed freshness transition with the previous public stamp.
#[derive(Clone)]
pub(crate) struct ActiveGenerationAuthority {
    active: Arc<RwLock<Option<Arc<ActiveGeneration>>>>,
}

impl fmt::Debug for ActiveGenerationAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveGenerationAuthority")
            .field(
                "active",
                &self
                    .snapshot()
                    .ok()
                    .flatten()
                    .map(|generation| generation.stamp().clone()),
            )
            .finish()
    }
}

impl ActiveGenerationAuthority {
    fn new(active: Option<Arc<ActiveGeneration>>) -> Self {
        Self {
            active: Arc::new(RwLock::new(active)),
        }
    }

    pub(crate) fn snapshot(&self) -> Result<Option<Arc<ActiveGeneration>>, PipelineError> {
        self.with_snapshot(|active| active.cloned())
    }

    pub(crate) fn with_snapshot<R>(
        &self,
        inspect: impl FnOnce(Option<&Arc<ActiveGeneration>>) -> R,
    ) -> Result<R, PipelineError> {
        let active = self.active.read().map_err(|_| {
            PipelineError::Invariant("active-generation authority lock is poisoned")
        })?;
        Ok(inspect(active.as_ref()))
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, Option<Arc<ActiveGeneration>>>, PipelineError> {
        self.active
            .write()
            .map_err(|_| PipelineError::Invariant("active-generation authority lock is poisoned"))
    }
}

fn validate_projection_summary(
    summary: GenerationProjectionSummary,
    state: &SourceStateSnapshot,
    readers: &ProjectionReaders,
) -> Result<(), PipelineError> {
    let actual_assets = u64::try_from(state.assets().len())
        .map_err(|_| PipelineError::ArithmeticOverflow("indexed asset count"))?;
    validate_summary_count("assets", summary.assets(), actual_assets)?;
    validate_summary_count(
        "search documents",
        summary.search_documents(),
        readers.search().reader().searcher().num_docs(),
    )?;
    validate_summary_count(
        "reference documents",
        summary.reference_documents(),
        readers.references().reader().searcher().num_docs(),
    )?;
    let actual_incomplete = u64::try_from(
        state
            .assets()
            .iter()
            .filter(|analysis| !analysis.complete)
            .count(),
    )
    .map_err(|_| PipelineError::ArithmeticOverflow("incomplete asset count"))?;
    validate_summary_count(
        "incomplete assets",
        summary.incomplete_assets(),
        actual_incomplete,
    )
}

fn validate_projection_summary_without_source_state(
    summary: GenerationProjectionSummary,
    readers: &ProjectionReaders,
) -> Result<(), PipelineError> {
    validate_summary_count(
        "search documents",
        summary.search_documents(),
        readers.search().reader().searcher().num_docs(),
    )?;
    validate_summary_count(
        "reference documents",
        summary.reference_documents(),
        readers.references().reader().searcher().num_docs(),
    )
}

fn validate_summary_count(
    resource: &'static str,
    manifest: u64,
    actual: u64,
) -> Result<(), PipelineError> {
    if manifest == actual {
        Ok(())
    } else {
        Err(PipelineError::GenerationSummaryMismatch {
            resource,
            manifest,
            actual,
        })
    }
}

fn suggestion_paths_from_state(
    state: &SourceStateSnapshot,
    options: SearchIndexOptions,
) -> impl Iterator<Item = &str> {
    state.assets().iter().flat_map(move |analysis| {
        let container_limit = if options.index_bundle_container_entries {
            options.max_bundle_container_entries_per_bundle
        } else {
            0
        };
        std::iter::once(analysis.source.relative_path.as_str()).chain(
            analysis
                .container_entries
                .iter()
                .take(container_limit)
                .map(|entry| entry.asset_path.as_str()),
        )
    })
}

fn projection_is_complete(state: &SourceStateSnapshot, options: SearchIndexOptions) -> bool {
    state
        .assets()
        .iter()
        .all(|analysis| analysis.references.len() <= options.max_references_per_asset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceStateAvailability {
    Reusable,
    OpaqueLegacy,
    IncompatibleSemantics,
}

impl SourceStateAvailability {
    fn classify(snapshot: Option<&GenerationSnapshot>, semantic_layout_matches: bool) -> Self {
        match snapshot {
            Some(snapshot) if !snapshot.source_state_storage_current() => Self::OpaqueLegacy,
            Some(_) if !semantic_layout_matches => Self::IncompatibleSemantics,
            _ => Self::Reusable,
        }
    }

    const fn is_reusable(self) -> bool {
        matches!(self, Self::Reusable)
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanValidationCheckpoint {
    NoChangePreReturn,
    ActivationPreCommit,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DesiredRevisionCommitCheckpoint {
    AfterStoreCommitBeforePublicInstall,
}

#[cfg(test)]
struct OneShotCheckpointHook<C> {
    checkpoint: C,
    action: Box<dyn FnOnce() + Send + 'static>,
}

#[cfg(test)]
impl<C: PartialEq> OneShotCheckpointHook<C> {
    fn run(hook: &mut Option<Self>, checkpoint: C) {
        if let Some(hook) = hook.take_if(|hook| hook.checkpoint == checkpoint) {
            (hook.action)();
        }
    }
}

pub(crate) struct GenerationAuthorityConfig<'paths> {
    paths: &'paths IndexPaths,
    options: SearchIndexOptions,
    options_digest: DigestV1,
    semantics: SearchSemantics,
    analysis_cache_identity: AnalysisCacheIdentityV1,
}

impl<'paths> GenerationAuthorityConfig<'paths> {
    pub(crate) fn new(
        paths: &'paths IndexPaths,
        options: SearchIndexOptions,
        semantics: SearchSemantics,
    ) -> Result<Self, PipelineError> {
        let options = options.validate().map_err(PipelineError::Configuration)?;
        let options_digest = paths
            .logical_configuration_digest(options)
            .map_err(PipelineError::Configuration)?;
        let analysis_cache_identity = semantics
            .analysis_cache_identity(options_digest)
            .map_err(|error| PipelineError::Configuration(anyhow::Error::new(error)))?;
        Ok(Self {
            paths,
            options,
            options_digest,
            semantics,
            analysis_cache_identity,
        })
    }

    pub(crate) const fn options(&self) -> SearchIndexOptions {
        self.options
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FilesystemReusePolicy {
    pub(crate) force_full_scan: bool,
    pub(crate) force_full_analysis: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceChangeAdmission {
    Apply,
    AlreadyApplied,
    ObserveCurrent,
}

pub(crate) enum SourceScanHintUpdate {
    Delete(IndexedSourceCoordinate),
    Upsert(SourceScanHint),
}

impl SourceScanHintUpdate {
    const fn coordinate(&self) -> IndexedSourceCoordinate {
        match self {
            Self::Delete(coordinate) => *coordinate,
            Self::Upsert(hint) => hint.coordinate,
        }
    }
}

enum GenerationReuse {
    Fresh,
    Incremental(SourceStateSnapshot),
    FullWithReusableAnalysis(SourceStateSnapshot),
    FullWithDiscardedAnalysis(Option<SourceStateSnapshot>),
    FullWithoutBaseline,
}

impl GenerationReuse {
    fn classify(
        source_state: Option<SourceStateSnapshot>,
        analysis_reusable: bool,
        incrementally_compatible: bool,
    ) -> Result<Self, PipelineError> {
        match (source_state, analysis_reusable, incrementally_compatible) {
            (Some(state), true, true) => Ok(Self::Incremental(state)),
            (None, true, true) => Ok(Self::Fresh),
            (Some(state), true, false) => Ok(Self::FullWithReusableAnalysis(state)),
            (state, false, false) => Ok(Self::FullWithDiscardedAnalysis(state)),
            (None, true, false) => Ok(Self::FullWithoutBaseline),
            (Some(_), false, true) | (None, false, true) => Err(PipelineError::Invariant(
                "incremental generation cannot discard its analysis baseline",
            )),
        }
    }

    const fn source_state(&self) -> Option<&SourceStateSnapshot> {
        match self {
            Self::Incremental(state)
            | Self::FullWithReusableAnalysis(state)
            | Self::FullWithDiscardedAnalysis(Some(state)) => Some(state),
            Self::Fresh | Self::FullWithDiscardedAnalysis(None) | Self::FullWithoutBaseline => None,
        }
    }

    fn reusable_assets(&self) -> &[crate::analysis::AssetAnalysis] {
        match self {
            Self::Incremental(state) | Self::FullWithReusableAnalysis(state) => state.assets(),
            Self::Fresh | Self::FullWithDiscardedAnalysis(_) | Self::FullWithoutBaseline => &[],
        }
    }

    fn scan_hints(&self) -> &[SourceScanHint] {
        self.source_state()
            .map_or(&[], SourceStateSnapshot::scan_hints)
    }

    const fn incrementally_compatible(&self) -> bool {
        matches!(self, Self::Fresh | Self::Incremental(_))
    }

    const fn analysis_reusable(&self) -> bool {
        !matches!(self, Self::FullWithDiscardedAnalysis(_))
    }
}

struct ReusableAnalysisEntry {
    coordinate: IndexedSourceCoordinate,
    analysis: Option<AssetAnalysis>,
}

pub(crate) struct ReusableAnalysisCache {
    entries: Vec<ReusableAnalysisEntry>,
    workspace_roots: Vec<(SourceId, usize)>,
}

impl ReusableAnalysisCache {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            workspace_roots: Vec::new(),
        }
    }

    pub(crate) fn get(&self, coordinate: IndexedSourceCoordinate) -> Option<&AssetAnalysis> {
        let index = self
            .entries
            .binary_search_by_key(&coordinate, |entry| entry.coordinate)
            .ok()?;
        self.entries[index].analysis.as_ref()
    }

    pub(crate) fn contains_key(&self, coordinate: IndexedSourceCoordinate) -> bool {
        self.get(coordinate).is_some()
    }

    pub(crate) fn take(&mut self, coordinate: IndexedSourceCoordinate) -> Option<AssetAnalysis> {
        let index = self
            .entries
            .binary_search_by_key(&coordinate, |entry| entry.coordinate)
            .ok()?;
        self.entries[index].analysis.take()
    }

    pub(crate) fn remove(&mut self, coordinate: IndexedSourceCoordinate) {
        let _ = self.take(coordinate);
    }

    pub(crate) fn take_workspace_root(&mut self, root: SourceId) -> Option<AssetAnalysis> {
        let index = self
            .workspace_roots
            .binary_search_by_key(&root, |(candidate, _)| *candidate)
            .ok()
            .map(|index| self.workspace_roots[index].1)?;
        self.entries[index].analysis.take()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &AssetAnalysis> {
        self.entries
            .iter()
            .filter_map(|entry| entry.analysis.as_ref())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (IndexedSourceCoordinate, &AssetAnalysis)> {
        self.entries.iter().filter_map(|entry| {
            entry
                .analysis
                .as_ref()
                .map(|analysis| (entry.coordinate, analysis))
        })
    }

    pub(crate) fn retain(
        &mut self,
        mut predicate: impl FnMut(IndexedSourceCoordinate, &AssetAnalysis) -> bool,
    ) {
        for entry in &mut self.entries {
            let retain = entry
                .analysis
                .as_ref()
                .is_none_or(|analysis| predicate(entry.coordinate, analysis));
            if !retain {
                let _ = entry.analysis.take();
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.values().count()
    }

    pub(crate) fn into_remaining(
        self,
    ) -> impl Iterator<Item = (IndexedSourceCoordinate, AssetAnalysis)> {
        self.entries
            .into_iter()
            .filter_map(|entry| entry.analysis.map(|analysis| (entry.coordinate, analysis)))
    }
}

pub(crate) struct GenerationPublicationInput {
    batch: AssetAnalysisBatch,
    scan_hints: Vec<SourceScanHint>,
    transaction_receipts: TransactionReceiptWindow,
    observation: PublicationObservation,
}

impl GenerationPublicationInput {
    pub(crate) fn asset_count(&self) -> usize {
        self.batch.assets.len()
    }
}

pub(crate) struct GenerationCandidate {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    assets: Vec<AssetAnalysis>,
    metrics: AnalysisMetrics,
    scan_hints: CandidateScanHints,
}

impl GenerationCandidate {
    pub(crate) fn filesystem(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        assets: Vec<AssetAnalysis>,
        metrics: AnalysisMetrics,
        scan_hint_updates: Vec<SourceScanHintUpdate>,
    ) -> Self {
        Self {
            workspace,
            revision,
            assets,
            metrics,
            scan_hints: CandidateScanHints::Updates(scan_hint_updates),
        }
    }

    pub(crate) fn workspace(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        assets: Vec<AssetAnalysis>,
        metrics: AnalysisMetrics,
        scan_hint_replacements: Vec<SourceScanHint>,
    ) -> Self {
        Self {
            workspace,
            revision,
            assets,
            metrics,
            scan_hints: CandidateScanHints::Replacements(scan_hint_replacements),
        }
    }
}

enum CandidateScanHints {
    Preserve,
    Updates(Vec<SourceScanHintUpdate>),
    Replacements(Vec<SourceScanHint>),
}

enum PublicationObservation {
    Workspace,
    Filesystem(ScanValidation),
}

#[derive(Clone, Copy)]
enum ScanHintRetention {
    All,
    IndexedAssetsOnly,
}

impl PublicationObservation {
    const fn filesystem_validation(&self) -> Option<&ScanValidation> {
        match self {
            Self::Workspace => None,
            Self::Filesystem(validation) => Some(validation),
        }
    }
}

pub(crate) enum GenerationPublicationResult {
    NoChange {
        active: Arc<ActiveGeneration>,
        warnings: Vec<String>,
    },
    Published {
        active: Arc<ActiveGeneration>,
        projection_metrics: ProjectionMetrics,
        disk_estimate: GenerationDiskEstimate,
        warnings: Vec<String>,
    },
}

pub(crate) struct SearchGenerationAuthority {
    options: SearchIndexOptions,
    options_digest: DigestV1,
    semantics: SearchSemantics,
    analysis_cache_identity: AnalysisCacheIdentityV1,
    store: GenerationStore,
    reuse: GenerationReuse,
    active: ActiveGenerationAuthority,
    rebuild_bootstrap: Option<GenerationRebuildBootstrap>,
    maintenance: GenerationMaintenanceStatus,
    pending_warnings: Vec<String>,
    pending_warnings_omitted: bool,
    #[cfg(test)]
    publish_failpoint: Option<GenerationFailpoint>,
    #[cfg(test)]
    scan_validation_hook: Option<OneShotCheckpointHook<ScanValidationCheckpoint>>,
    #[cfg(test)]
    desired_revision_commit_hook: Option<OneShotCheckpointHook<DesiredRevisionCommitCheckpoint>>,
}

impl fmt::Debug for SearchGenerationAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchGenerationAuthority")
            .field("analysis_reusable", &self.reuse.analysis_reusable())
            .field(
                "incrementally_compatible",
                &self.reuse.incrementally_compatible(),
            )
            .field(
                "active_generation",
                &self
                    .active
                    .snapshot()
                    .ok()
                    .flatten()
                    .map(|active| active.stamp().clone()),
            )
            .field("maintenance", &self.maintenance)
            .finish_non_exhaustive()
    }
}

impl SearchGenerationAuthority {
    pub(crate) fn open(
        config: GenerationAuthorityConfig<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PipelineError> {
        Self::open_configured(
            config,
            budget,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn open_with_startup_recovery_failpoint(
        config: GenerationAuthorityConfig<'_>,
        failpoint: GenerationFailpoint,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PipelineError> {
        Self::open_configured(config, budget, Some(failpoint))
    }

    fn open_configured(
        config: GenerationAuthorityConfig<'_>,
        budget: &mut AssetLoadBudget,
        #[cfg(test)] startup_recovery_failpoint: Option<GenerationFailpoint>,
    ) -> Result<Self, PipelineError> {
        let store_options = GenerationStoreOptions {
            retain_previous_generations: config.options.retain_previous_generations,
        };
        #[cfg(not(test))]
        let opened = GenerationStore::open_private(
            config.paths.private_index_root().clone(),
            store_options,
            budget,
        )?;
        #[cfg(test)]
        let opened = match startup_recovery_failpoint {
            Some(failpoint) => GenerationStore::open_private_with_startup_recovery_failpoint(
                config.paths.private_index_root().clone(),
                store_options,
                budget,
                failpoint,
            )?,
            None => GenerationStore::open_private(
                config.paths.private_index_root().clone(),
                store_options,
                budget,
            )?,
        };
        let (store, staging_recovery, startup_disposition) = opened.into_parts();
        let rebuild_bootstrap = startup_disposition
            .rebuild_required()
            .map(|required| required.bootstrap().clone());
        let maintenance = match staging_recovery {
            Ok(report) => GenerationMaintenanceStatus {
                last_recovered_entries: report.removed_entries(),
                ..GenerationMaintenanceStatus::clean()
            },
            Err(error) => GenerationMaintenanceStatus {
                state: GenerationMaintenanceState::RecoveryRequired,
                last_recovered_entries: 0,
                last_cleanup_failure: Some(crate::wire::bounded_error_message(error.to_string())),
            },
        };
        let recovered = store.active().cloned();
        let (active_semantics_match, active_configuration_match, active_source_state_layout_match) =
            recovered.as_ref().map_or((true, true, true), |snapshot| {
                let persisted_semantics = snapshot.manifest().semantics();
                let configuration_match =
                    snapshot.manifest().options_digest() == config.options_digest;
                (
                    persisted_semantics == config.semantics,
                    configuration_match,
                    persisted_semantics.source_state_layout_compatible_with(config.semantics),
                )
            });
        let source_state_availability =
            SourceStateAvailability::classify(recovered.as_ref(), active_source_state_layout_match);
        let source_state = if source_state_availability.is_reusable() {
            recovered
                .as_ref()
                .map(|snapshot| snapshot.load_source_state(budget))
                .transpose()?
        } else {
            None
        };
        if let Some(source_state) = source_state.as_ref() {
            source_state.validate_project_path_space(config.paths.project_path_space())?;
        }
        let analysis_reusable = match (recovered.as_ref(), source_state.as_ref()) {
            (Some(_), Some(state)) => {
                state.analysis_cache_identity() == config.analysis_cache_identity
            }
            (Some(_), None) => false,
            (None, None) => true,
            (None, Some(_)) => {
                return Err(PipelineError::Invariant(
                    "source state exists without an activated generation",
                ));
            }
        };
        let mut incrementally_compatible = if recovered.is_some() {
            active_semantics_match
                && active_configuration_match
                && source_state_availability.is_reusable()
        } else {
            startup_disposition.rebuild_required().is_none()
        };
        let active = match (recovered, source_state.as_ref()) {
            (Some(snapshot), Some(state)) => {
                match ProjectionReaders::open(snapshot.directory(), budget) {
                    Ok(readers) => Some(Arc::new(ActiveGeneration::open(
                        snapshot,
                        state,
                        &readers,
                        None,
                        config.options,
                        active_semantics_match && source_state_availability.is_reusable(),
                        active_configuration_match,
                        budget,
                    )?)),
                    Err(error) if is_rebuildable_projection_schema_version(&error) => {
                        incrementally_compatible = false;
                        None
                    }
                    Err(error) => return Err(PipelineError::Projection(error)),
                }
            }
            (Some(snapshot), None) if !source_state_availability.is_reusable() => {
                match ProjectionReaders::open(snapshot.directory(), budget) {
                    Ok(readers) => Some(Arc::new(ActiveGeneration::open_projection_only(
                        snapshot,
                        &readers,
                        false,
                        active_configuration_match,
                        budget,
                    )?)),
                    Err(error) if is_rebuildable_projection_schema_version(&error) => {
                        incrementally_compatible = false;
                        None
                    }
                    Err(error) => return Err(PipelineError::Projection(error)),
                }
            }
            (None, None) => None,
            (Some(_), None) => {
                return Err(PipelineError::Invariant(
                    "compatible activated generation is missing its source state",
                ));
            }
            (None, Some(_)) => {
                return Err(PipelineError::Invariant(
                    "source state exists without an activated generation",
                ));
            }
        };
        let reuse =
            GenerationReuse::classify(source_state, analysis_reusable, incrementally_compatible)?;
        Ok(Self {
            options: config.options,
            options_digest: config.options_digest,
            semantics: config.semantics,
            analysis_cache_identity: config.analysis_cache_identity,
            store,
            reuse,
            active: ActiveGenerationAuthority::new(active),
            rebuild_bootstrap,
            maintenance,
            pending_warnings: Vec::new(),
            pending_warnings_omitted: false,
            #[cfg(test)]
            publish_failpoint: None,
            #[cfg(test)]
            scan_validation_hook: None,
            #[cfg(test)]
            desired_revision_commit_hook: None,
        })
    }

    pub(crate) fn workspace_id_hint(&self) -> Option<WorkspaceId> {
        self.reuse
            .source_state()
            .map(SourceStateSnapshot::workspace)
            .or_else(|| {
                self.store
                    .active()
                    .map(|snapshot| snapshot.manifest().workspace())
            })
            .or_else(|| {
                self.rebuild_bootstrap
                    .as_ref()
                    .map(GenerationRebuildBootstrap::workspace)
            })
    }

    pub(crate) fn active(&self) -> Result<Option<Arc<ActiveGeneration>>, PipelineError> {
        self.active.snapshot()
    }

    pub(crate) fn active_authority(&self) -> ActiveGenerationAuthority {
        self.active.clone()
    }

    pub(crate) fn maintenance(&self) -> GenerationMaintenanceStatus {
        self.maintenance.clone()
    }

    pub(crate) const fn filesystem_reuse_policy(
        &self,
        workspace_hydrated: bool,
    ) -> FilesystemReusePolicy {
        FilesystemReusePolicy {
            force_full_scan: !workspace_hydrated || !self.reuse.incrementally_compatible(),
            force_full_analysis: !self.reuse.analysis_reusable(),
        }
    }

    pub(crate) fn reusable_analysis_cache(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReusableAnalysisCache, PipelineError> {
        clone_reusable_analysis_cache(self.reuse.reusable_assets(), budget)
    }

    pub(crate) fn known_project_paths(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<ProjectSourcePath>, PipelineError> {
        clone_known_project_paths(
            self.reuse
                .source_state()
                .map_or(&[], SourceStateSnapshot::assets),
            budget,
        )
    }

    fn indexed_workspace_revision(&self) -> Option<(WorkspaceId, WorkspaceRevision)> {
        self.reuse
            .source_state()
            .map(|state| (state.workspace(), state.revision()))
    }

    pub(crate) fn admit_workspace_change(
        &mut self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceChangeAdmission, PipelineError> {
        if !self.reuse.incrementally_compatible() {
            return Err(PipelineError::FilesystemReindexRequired);
        }
        let Some((workspace, revision)) = self.indexed_workspace_revision() else {
            return Ok(WorkspaceChangeAdmission::Apply);
        };
        if workspace != changes.workspace() {
            return Err(PipelineError::WorkspaceMismatch {
                expected: workspace,
                actual: changes.workspace(),
            });
        }
        match self.receipt_membership(changes, budget)? {
            TransactionReceiptMembership::Exact => {
                return Ok(WorkspaceChangeAdmission::AlreadyApplied);
            }
            TransactionReceiptMembership::Conflict => {
                return Err(PipelineError::TransactionConflict {
                    transaction: changes.transaction(),
                });
            }
            TransactionReceiptMembership::Absent => {}
        }
        if revision != changes.from_revision() && revision != changes.to_revision() {
            return Err(PipelineError::RevisionBarrierMismatch {
                indexed: revision,
                change_from: changes.from_revision(),
                change_to: changes.to_revision(),
            });
        }
        if revision == changes.to_revision() {
            Ok(WorkspaceChangeAdmission::ObserveCurrent)
        } else {
            self.observe_desired_revision(changes.to_revision(), budget)?;
            Ok(WorkspaceChangeAdmission::Apply)
        }
    }

    fn transaction_receipts(&self) -> Option<&TransactionReceiptWindow> {
        self.store
            .active()
            .map(GenerationSnapshot::transaction_receipts)
    }

    pub(crate) fn actual_revision_is(&self, revision: WorkspaceRevision) -> bool {
        self.store
            .active()
            .is_some_and(|active| active.manifest().revision() == revision)
    }

    fn receipt_membership(
        &self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<TransactionReceiptMembership, PipelineError> {
        Ok(self
            .transaction_receipts()
            .ok_or(PipelineError::Invariant(
                "active generation disappeared during receipt lookup",
            ))?
            .membership(changes, budget)?)
    }

    fn filesystem_transaction_receipts(&self, workspace: WorkspaceId) -> TransactionReceiptWindow {
        match self.store.active() {
            Some(active) if active.manifest().workspace() == workspace => {
                active.transaction_receipts().clone()
            }
            None if self
                .rebuild_bootstrap
                .as_ref()
                .is_some_and(|bootstrap| bootstrap.workspace() == workspace) =>
            {
                self.rebuild_bootstrap
                    .as_ref()
                    .expect("matching rebuild bootstrap disappeared")
                    .transaction_receipts()
                    .clone()
            }
            Some(_) | None => TransactionReceiptWindow::empty(),
        }
    }

    pub(crate) fn prepare_observed_publication(
        &self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublicationInput, PipelineError> {
        let (workspace, revision) =
            self.indexed_workspace_revision()
                .ok_or(PipelineError::Invariant(
                    "observed transaction requires indexed source state",
                ))?;
        if workspace != changes.workspace() || revision != changes.to_revision() {
            return Err(PipelineError::Invariant(
                "observed transaction does not match indexed workspace revision",
            ));
        }
        let receipts = self
            .transaction_receipts()
            .ok_or(PipelineError::Invariant(
                "active generation disappeared during reconciled receipt update",
            ))?
            .after_reconciled_target(workspace, revision, changes, budget)?;
        let assets = clone_reusable_analyses(self.reuse.reusable_assets(), budget)?;
        let candidate = GenerationCandidate {
            workspace,
            revision,
            assets,
            metrics: AnalysisMetrics::default(),
            scan_hints: CandidateScanHints::Preserve,
        };
        self.prepare_publication_input(
            candidate,
            receipts,
            PublicationObservation::Workspace,
            ScanHintRetention::All,
            budget,
        )
    }

    pub(crate) fn prepare_workspace_publication(
        &self,
        changes: &ChangeSet,
        candidate: GenerationCandidate,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublicationInput, PipelineError> {
        if candidate.workspace != changes.workspace() || candidate.revision != changes.to_revision()
        {
            return Err(PipelineError::Invariant(
                "workspace publication candidate does not match its change set",
            ));
        }
        let receipts = match (
            self.indexed_workspace_revision(),
            self.transaction_receipts(),
        ) {
            (Some((indexed_workspace, indexed_revision)), Some(receipts)) => {
                receipts.after_change_set(indexed_workspace, indexed_revision, changes, budget)?
            }
            (None, None) => TransactionReceiptWindow::from_change_set(changes, budget)?,
            _ => {
                return Err(PipelineError::Invariant(
                    "generation and source-state lifecycle diverged during receipt update",
                ));
            }
        };
        self.prepare_publication_input(
            candidate,
            receipts,
            PublicationObservation::Workspace,
            ScanHintRetention::IndexedAssetsOnly,
            budget,
        )
    }

    pub(crate) fn prepare_filesystem_publication(
        &self,
        candidate: GenerationCandidate,
        validation: ScanValidation,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublicationInput, PipelineError> {
        let receipts = self.filesystem_transaction_receipts(candidate.workspace);
        self.prepare_publication_input(
            candidate,
            receipts,
            PublicationObservation::Filesystem(validation),
            ScanHintRetention::All,
            budget,
        )
    }

    fn prepare_publication_input(
        &self,
        candidate: GenerationCandidate,
        transaction_receipts: TransactionReceiptWindow,
        observation: PublicationObservation,
        retention: ScanHintRetention,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublicationInput, PipelineError> {
        let GenerationCandidate {
            workspace,
            revision,
            assets,
            metrics,
            scan_hints,
        } = candidate;
        let mut scan_hints = match scan_hints {
            CandidateScanHints::Preserve => clone_scan_hints(self.reuse.scan_hints(), budget)?,
            CandidateScanHints::Updates(updates) => {
                merge_scan_hint_updates(self.reuse.scan_hints(), updates, budget)?
            }
            CandidateScanHints::Replacements(replacements) => {
                merge_scan_hint_replacements(self.reuse.scan_hints(), replacements, budget)?
            }
        };
        let transactions = canonical_transaction_ids(&transaction_receipts, budget)?;
        let batch = AssetAnalysisBatch::new(workspace, revision, transactions, assets, metrics);
        if matches!(retention, ScanHintRetention::IndexedAssetsOnly) {
            retain_scan_hints_for_assets(&mut scan_hints, &batch.assets);
        }
        Ok(GenerationPublicationInput {
            batch,
            scan_hints,
            transaction_receipts,
            observation,
        })
    }

    pub(crate) fn ensure_operational(&mut self) -> Result<(), PipelineError> {
        match self.store.ensure_operational() {
            Ok(()) => Ok(()),
            Err(error) => Err(self.fail_store_operation(error)),
        }
    }

    pub(crate) fn recover(&mut self) -> Result<(), PipelineError> {
        self.recover_inner(
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn recover_with_failpoint(
        &mut self,
        failpoint: GenerationFailpoint,
    ) -> Result<(), PipelineError> {
        self.recover_inner(Some(failpoint))
    }

    fn recover_inner(
        &mut self,
        #[cfg(test)] failpoint: Option<GenerationFailpoint>,
    ) -> Result<(), PipelineError> {
        let mut cleanup_budget = AssetLoadBudget::default();
        #[cfg(not(test))]
        let recovery = self.store.reconcile_abandoned_staging(&mut cleanup_budget);
        #[cfg(test)]
        let recovery = match failpoint {
            Some(failpoint) => self
                .store
                .reconcile_abandoned_staging_with_failpoint(&mut cleanup_budget, failpoint),
            None => self.store.reconcile_abandoned_staging(&mut cleanup_budget),
        };
        match recovery {
            Ok(report) => {
                self.maintenance = GenerationMaintenanceStatus {
                    last_recovered_entries: report.removed_entries(),
                    ..GenerationMaintenanceStatus::clean()
                };
                Ok(())
            }
            Err(error) => {
                self.record_cleanup_failure(error.to_string());
                if error.requires_reopen() {
                    *self.active.write()? = None;
                }
                Err(PipelineError::Store(Box::new(error)))
            }
        }
    }

    pub(crate) fn observe_desired_revision(
        &mut self,
        desired: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PipelineError> {
        let active_authority = self.active.clone();
        let mut public_active = active_authority.write()?;
        let commit = match self.store.record_desired_revision(desired, budget) {
            Ok(commit) => commit,
            Err(error) => {
                if error.requires_reopen() {
                    *public_active = None;
                }
                drop(public_active);
                self.record_store_reopen_requirement(&error);
                return Err(PipelineError::Store(Box::new(error)));
            }
        };
        #[cfg(test)]
        OneShotCheckpointHook::run(
            &mut self.desired_revision_commit_hook,
            DesiredRevisionCommitCheckpoint::AfterStoreCommitBeforePublicInstall,
        );
        let warnings = match commit {
            DesiredRevisionCommit::NoActive => {
                if public_active.is_some() {
                    *public_active = None;
                    drop(public_active);
                    return Err(PipelineError::Invariant(
                        "durable desired-revision result disagrees with public active generation",
                    ));
                }
                Vec::new()
            }
            DesiredRevisionCommit::Unchanged => {
                if public_active.is_none() {
                    *public_active = None;
                    drop(public_active);
                    return Err(PipelineError::Invariant(
                        "durable desired-revision result disagrees with public active generation",
                    ));
                }
                Vec::new()
            }
            DesiredRevisionCommit::Committed(report) => {
                let active = public_active.as_mut().ok_or(PipelineError::Invariant(
                    "durable desired-revision result disagrees with public active generation",
                ))?;
                if let Err(error) = Arc::make_mut(active).install_committed_snapshot(report.active)
                {
                    *public_active = None;
                    drop(public_active);
                    return Err(error);
                }
                report.warnings
            }
        };
        drop(public_active);
        self.append_pending_warnings(warnings);
        Ok(())
    }

    pub(crate) fn publish(
        &mut self,
        scanner: &ProjectScanner,
        input: GenerationPublicationInput,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublicationResult, PipelineError> {
        let GenerationPublicationInput {
            batch,
            scan_hints,
            transaction_receipts,
            observation,
        } = input;
        if self.reuse.incrementally_compatible()
            && self.store.active().is_some_and(|active| {
                active
                    .transaction_receipts()
                    .matches_canonical_ids(&batch.transactions)
            })
            && self
                .reuse
                .source_state()
                .is_some_and(|state| batch_matches_state(&batch, state))
        {
            if let Some(validation) = observation.filesystem_validation() {
                #[cfg(test)]
                OneShotCheckpointHook::run(
                    &mut self.scan_validation_hook,
                    ScanValidationCheckpoint::NoChangePreReturn,
                );
                scanner
                    .validate_scan(validation)
                    .map_err(|error| PipelineError::Scan(Box::new(error)))?;
            }
            let active = self.active()?.ok_or(PipelineError::Invariant(
                "unchanged generation is missing its public active authority",
            ))?;
            return Ok(GenerationPublicationResult::NoChange {
                active,
                warnings: self.take_pending_warnings(),
            });
        }

        let projection = project_batch(
            &batch,
            ProjectionLimits {
                max_references_per_asset: self.options.max_references_per_asset,
                max_container_entries_per_asset: if self.options.index_bundle_container_entries {
                    self.options.max_bundle_container_entries_per_bundle
                } else {
                    0
                },
            },
            budget,
        )
        .map_err(|error| match error {
            ProjectionError::Budget(error) => PipelineError::Budget(error),
            error => PipelineError::Projection(error.into()),
        })?;
        let projection_metrics = projection.metrics;
        let source_state =
            SourceStateSnapshot::from_batch(batch, scan_hints, self.analysis_cache_identity)?;
        let mut build = self.store.begin()?;
        let staged = (|| -> Result<_, PipelineError> {
            let projection_evidence = ProjectionStore::build(build.directory(), &projection)
                .map_err(PipelineError::Projection)?;
            build.write_source_state(&source_state)?;

            {
                let readers = ProjectionReaders::open(build.directory(), budget)
                    .map_err(PipelineError::Projection)?;
                SearchQueryFields::from_schema(&readers.search().index().schema())
                    .map_err(PipelineError::Query)?;
            }

            let artifacts = self.store.measure_artifacts_with_budget(&build, budget)?;
            let expected_artifacts =
                projection_evidence.generation_artifacts(artifacts.source_state());
            if artifacts != expected_artifacts {
                return Err(PipelineError::Invariant(
                    "projection evidence changed before generation publication",
                ));
            }
            let projection_summary = GenerationProjectionSummary::new(
                u64::try_from(source_state.assets().len())
                    .map_err(|_| PipelineError::ArithmeticOverflow("indexed asset count"))?,
                projection.metrics.search_documents,
                projection.metrics.reference_documents,
                u64::try_from(projection.truncations.len()).map_err(|_| {
                    PipelineError::ArithmeticOverflow("projection truncation count")
                })?,
                u64::try_from(
                    source_state
                        .assets()
                        .iter()
                        .filter(|analysis| !analysis.complete)
                        .count(),
                )
                .map_err(|_| PipelineError::ArithmeticOverflow("incomplete asset count"))?,
            )?;
            let identity = SearchGenerationIdentityV1::new_with_semantics(
                source_state.workspace(),
                source_state.revision(),
                GenerationProjectionDigests::new(
                    projection_evidence.logical_digests().search(),
                    projection_evidence.logical_digests().references(),
                ),
                projection_summary,
                self.semantics,
                self.options_digest,
                source_state.logical_digest(),
            )?;
            let manifest = SearchGenerationManifestV1::new(identity, artifacts);
            let desired_revision = self
                .store
                .active()
                .filter(|active| active.manifest().revision() == manifest.revision())
                .map(GenerationSnapshot::desired_revision)
                .or_else(|| {
                    self.rebuild_bootstrap
                        .as_ref()
                        .filter(|bootstrap| {
                            bootstrap.workspace() == manifest.workspace()
                                && bootstrap.actual_revision() == manifest.revision()
                        })
                        .map(GenerationRebuildBootstrap::desired_revision)
                })
                .unwrap_or_else(|| manifest.revision());
            let disk_estimate = self.store.estimate_manifest_publish(&manifest, budget)?;
            Ok((manifest, desired_revision, disk_estimate))
        })();
        let (manifest, desired_revision, disk_estimate) = match staged {
            Ok(staged) => staged,
            Err(primary) => return Err(self.abort_build(&mut build, primary)),
        };
        let activation = GenerationActivationEvidence::new(
            self.store.active().map(GenerationSnapshot::generation),
            transaction_receipts,
        );
        #[cfg(test)]
        let prepared_result = match self.publish_failpoint.take() {
            Some(failpoint) => self
                .store
                .prepare_publish_with_desired_revision_failpoint_and_budget(
                    &mut build,
                    manifest,
                    activation,
                    desired_revision,
                    budget,
                    failpoint,
                ),
            None => self.store.prepare_publish_with_desired_revision_and_budget(
                &mut build,
                manifest,
                activation,
                desired_revision,
                budget,
            ),
        };
        #[cfg(not(test))]
        let prepared_result = self.store.prepare_publish_with_desired_revision_and_budget(
            &mut build,
            manifest,
            activation,
            desired_revision,
            budget,
        );
        let prepared = match prepared_result {
            Ok(prepared) => prepared,
            Err(error) => {
                let primary = PipelineError::Store(Box::new(error));
                return Err(self.abort_build(&mut build, primary));
            }
        };
        let candidate_snapshot = prepared.snapshot().clone();
        let readers = ProjectionReaders::open(candidate_snapshot.directory(), budget)
            .map_err(PipelineError::Projection)?;
        let active = Arc::new(ActiveGeneration::open(
            candidate_snapshot,
            &source_state,
            &readers,
            Some(&projection),
            self.options,
            true,
            true,
            budget,
        )?);
        if let Some(validation) = observation.filesystem_validation() {
            #[cfg(test)]
            OneShotCheckpointHook::run(
                &mut self.scan_validation_hook,
                ScanValidationCheckpoint::ActivationPreCommit,
            );
            scanner
                .validate_scan(validation)
                .map_err(|error| PipelineError::Scan(Box::new(error)))?;
        }
        let active_authority = self.active.clone();
        let mut public_active = active_authority.write()?;
        let report = match prepared.activate_with_budget(budget) {
            Ok(report) => report,
            Err(error) => {
                if error.requires_reopen() {
                    *public_active = None;
                }
                drop(public_active);
                self.record_store_reopen_requirement(&error);
                return Err(PipelineError::Store(Box::new(error)));
            }
        };
        let mut active = active;
        let committed_snapshot = report.active;
        let publish_warnings = report.warnings;
        if let Err(error) =
            Arc::make_mut(&mut active).install_committed_snapshot(committed_snapshot)
        {
            *public_active = None;
            drop(public_active);
            return Err(error);
        }
        *public_active = Some(Arc::clone(&active));
        drop(public_active);
        self.append_pending_warnings(publish_warnings);
        let warnings = self.take_pending_warnings();
        self.reuse = GenerationReuse::Incremental(source_state);
        self.rebuild_bootstrap = None;

        Ok(GenerationPublicationResult::Published {
            active,
            projection_metrics,
            disk_estimate,
            warnings,
        })
    }

    fn abort_build(
        &mut self,
        build: &mut GenerationBuild,
        primary: PipelineError,
    ) -> PipelineError {
        let mut cleanup_budget = AssetLoadBudget::default();
        match build.abort_with_budget(&mut cleanup_budget) {
            Ok(()) => primary,
            Err(cleanup) => {
                self.record_cleanup_failure(cleanup.to_string());
                PipelineError::StagingAbortFailed {
                    primary: Box::new(primary),
                    cleanup: Box::new(cleanup),
                }
            }
        }
    }

    fn fail_store_operation(&mut self, error: GenerationStoreError) -> PipelineError {
        if error.requires_reopen() {
            self.record_store_reopen_requirement(&error);
            let mut active = match self.active.write() {
                Ok(active) => active,
                Err(lock_error) => return lock_error,
            };
            *active = None;
            return PipelineError::Store(Box::new(error));
        }
        PipelineError::Store(Box::new(error))
    }

    fn record_store_reopen_requirement(&mut self, error: &GenerationStoreError) {
        if error.requires_reopen() {
            self.record_cleanup_failure(error.to_string());
        }
    }

    fn record_cleanup_failure(&mut self, message: String) {
        self.maintenance = GenerationMaintenanceStatus {
            state: GenerationMaintenanceState::RecoveryRequired,
            last_recovered_entries: self.maintenance.last_recovered_entries,
            last_cleanup_failure: Some(crate::wire::bounded_error_message(message)),
        };
    }

    fn take_pending_warnings(&mut self) -> Vec<String> {
        const OMITTED_WARNING: &str =
            "additional publish warnings were omitted to satisfy the protocol budget";

        let mut warnings = std::mem::take(&mut self.pending_warnings);
        let mut omitted = std::mem::take(&mut self.pending_warnings_omitted);
        while ReindexEvidence::validate_publish_warnings(&warnings).is_err() {
            let _ = warnings.pop();
            omitted = true;
        }
        if omitted {
            loop {
                warnings.push(OMITTED_WARNING.to_owned());
                if ReindexEvidence::validate_publish_warnings(&warnings).is_ok() {
                    break;
                }
                let _ = warnings.pop();
                let _ = warnings.pop();
            }
        }
        warnings
    }

    pub(crate) fn append_pending_warnings(
        &mut self,
        warnings: impl IntoIterator<Item = GenerationPublishWarning>,
    ) {
        for warning in warnings {
            let kind = warning.kind();
            let cleanup = matches!(
                kind,
                GenerationPublishWarningKind::PreparationCleanup
                    | GenerationPublishWarningKind::PostCommitCleanup
            );
            let retain = self.pending_warnings.len() < MAX_REINDEX_PUBLISH_WARNINGS;
            if !cleanup && !retain {
                self.pending_warnings_omitted = true;
                continue;
            }
            let message = warning.to_string();
            if cleanup {
                self.record_cleanup_failure(message.clone());
            }
            if retain {
                self.pending_warnings
                    .push(crate::wire::bounded_publish_warning(message));
            } else {
                self.pending_warnings_omitted = true;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_publish_failpoint(&mut self, failpoint: GenerationFailpoint) {
        self.publish_failpoint = Some(failpoint);
    }

    #[cfg(test)]
    pub(crate) fn inject_scan_validation_hook(
        &mut self,
        checkpoint: ScanValidationCheckpoint,
        action: impl FnOnce() + Send + 'static,
    ) {
        self.scan_validation_hook = Some(OneShotCheckpointHook {
            checkpoint,
            action: Box::new(action),
        });
    }

    #[cfg(test)]
    pub(crate) fn inject_desired_revision_commit_hook(
        &mut self,
        checkpoint: DesiredRevisionCommitCheckpoint,
        action: impl FnOnce() + Send + 'static,
    ) {
        self.desired_revision_commit_hook = Some(OneShotCheckpointHook {
            checkpoint,
            action: Box::new(action),
        });
    }

    #[cfg(test)]
    pub(crate) fn poison_for_test(&mut self) {
        self.store.poison_activation_outcome_for_test();
    }

    #[cfg(test)]
    pub(crate) fn pending_warning_count(&self) -> usize {
        self.pending_warnings.len()
    }

    #[cfg(test)]
    pub(crate) fn rebuild_bootstrap(&self) -> Option<&GenerationRebuildBootstrap> {
        self.rebuild_bootstrap.as_ref()
    }

    #[cfg(test)]
    pub(crate) const fn pending_warnings_omitted(&self) -> bool {
        self.pending_warnings_omitted
    }

    #[cfg(test)]
    pub(crate) fn take_pending_warnings_for_test(&mut self) -> Vec<String> {
        self.take_pending_warnings()
    }
}

fn clone_reusable_analysis_cache(
    analyses: &[AssetAnalysis],
    budget: &mut AssetLoadBudget,
) -> Result<ReusableAnalysisCache, PipelineError> {
    if analyses.is_empty() {
        return Ok(ReusableAnalysisCache::empty());
    }
    for analysis in analyses {
        charge_cached_analysis_clone(analysis, budget)?;
    }
    let mut entries = reserve_retained_vec(analyses.len(), "cached asset index", budget)?;
    for analysis in analyses {
        entries.push(ReusableAnalysisEntry {
            coordinate: analysis.source.coordinate,
            analysis: Some(analysis.clone()),
        });
    }
    entries.sort_unstable_by_key(|entry| entry.coordinate);
    if entries
        .windows(2)
        .any(|pair| pair[0].coordinate == pair[1].coordinate)
    {
        return Err(PipelineError::Invariant(
            "cached assets contain duplicate source coordinates",
        ));
    }

    let workspace_root_count = entries
        .iter()
        .filter(|entry| {
            entry
                .analysis
                .as_ref()
                .and_then(|analysis| analysis.source.workspace_source)
                .is_some()
        })
        .count();
    let mut workspace_roots =
        reserve_retained_vec(workspace_root_count, "cached workspace root index", budget)?;
    for (index, entry) in entries.iter().enumerate() {
        if let Some(root) = entry
            .analysis
            .as_ref()
            .and_then(|analysis| analysis.source.workspace_source)
        {
            workspace_roots.push((root, index));
        }
    }
    workspace_roots.sort_unstable_by_key(|(root, _)| *root);
    if workspace_roots
        .windows(2)
        .any(|pair| pair[0].0 == pair[1].0)
    {
        return Err(PipelineError::Invariant(
            "cached assets contain duplicate workspace roots",
        ));
    }
    Ok(ReusableAnalysisCache {
        entries,
        workspace_roots,
    })
}

fn clone_reusable_analyses(
    analyses: &[AssetAnalysis],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<AssetAnalysis>, PipelineError> {
    for analysis in analyses {
        charge_cached_analysis_clone(analysis, budget)?;
    }
    let mut cloned = reserve_retained_vec(analyses.len(), "observed transaction assets", budget)?;
    cloned.extend(analyses.iter().cloned());
    Ok(cloned)
}

fn clone_known_project_paths(
    assets: &[AssetAnalysis],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ProjectSourcePath>, PipelineError> {
    let project_path_count = assets
        .iter()
        .filter(|analysis| analysis.source.coordinate.project_path().is_some())
        .count();
    let mut known_paths =
        reserve_retained_vec(project_path_count, "known source path index", budget)?;
    for analysis in assets {
        let Some(identity) = analysis.source.coordinate.project_path() else {
            continue;
        };
        charge_retained_string(&analysis.source.relative_path, "known source path", budget)?;
        let relative_path =
            clone_precharged_string(&analysis.source.relative_path, "known source path")?;
        known_paths.push(ProjectSourcePath::from_validated_parts(
            identity,
            relative_path,
        ));
    }
    known_paths.sort_unstable_by(|left, right| {
        compare_portable_paths(left.relative_path(), right.relative_path())
            .then_with(|| left.identity().cmp(&right.identity()))
    });
    Ok(known_paths)
}

fn clone_scan_hints(
    hints: &[SourceScanHint],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<SourceScanHint>, PipelineError> {
    let mut cloned = reserve_retained_vec(hints.len(), "source scan hints", budget)?;
    for hint in hints {
        push_cloned_scan_hint(&mut cloned, hint, budget)?;
    }
    Ok(cloned)
}

fn merge_scan_hint_updates(
    previous: &[SourceScanHint],
    mut updates: Vec<SourceScanHintUpdate>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<SourceScanHint>, PipelineError> {
    updates.sort_unstable_by_key(SourceScanHintUpdate::coordinate);
    if updates
        .windows(2)
        .any(|pair| pair[0].coordinate() == pair[1].coordinate())
    {
        return Err(PipelineError::Invariant(
            "filesystem scan produced duplicate scan hint updates",
        ));
    }

    let mut previous_index = 0;
    let mut final_count = 0_usize;
    for update in &updates {
        let coordinate = update.coordinate();
        while previous_index < previous.len() && previous[previous_index].coordinate < coordinate {
            final_count = final_count
                .checked_add(1)
                .ok_or(PipelineError::ArithmeticOverflow("source scan hint count"))?;
            previous_index += 1;
        }
        if previous_index < previous.len() && previous[previous_index].coordinate == coordinate {
            previous_index += 1;
        }
        if matches!(update, SourceScanHintUpdate::Upsert(_)) {
            final_count = final_count
                .checked_add(1)
                .ok_or(PipelineError::ArithmeticOverflow("source scan hint count"))?;
        }
    }
    final_count = final_count
        .checked_add(previous.len().saturating_sub(previous_index))
        .ok_or(PipelineError::ArithmeticOverflow("source scan hint count"))?;

    let mut merged = reserve_retained_vec(final_count, "source scan hints", budget)?;
    previous_index = 0;
    for update in updates {
        let coordinate = update.coordinate();
        while previous_index < previous.len() && previous[previous_index].coordinate < coordinate {
            push_cloned_scan_hint(&mut merged, &previous[previous_index], budget)?;
            previous_index += 1;
        }
        if previous_index < previous.len() && previous[previous_index].coordinate == coordinate {
            previous_index += 1;
        }
        if let SourceScanHintUpdate::Upsert(hint) = update {
            merged.push(hint);
        }
    }
    while previous_index < previous.len() {
        push_cloned_scan_hint(&mut merged, &previous[previous_index], budget)?;
        previous_index += 1;
    }
    debug_assert_eq!(merged.len(), final_count);
    Ok(merged)
}

fn merge_scan_hint_replacements(
    previous: &[SourceScanHint],
    mut replacements: Vec<SourceScanHint>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<SourceScanHint>, PipelineError> {
    replacements.sort_unstable_by_key(|hint| hint.coordinate);
    if replacements
        .windows(2)
        .any(|pair| pair[0].coordinate == pair[1].coordinate)
    {
        return Err(PipelineError::Invariant(
            "workspace reindex produced duplicate scan hint updates",
        ));
    }

    let mut previous_index = 0;
    let mut final_count = 0_usize;
    for replacement in &replacements {
        while previous_index < previous.len()
            && previous[previous_index].coordinate < replacement.coordinate
        {
            final_count = final_count
                .checked_add(1)
                .ok_or(PipelineError::ArithmeticOverflow("source scan hint count"))?;
            previous_index += 1;
        }
        if previous_index < previous.len()
            && previous[previous_index].coordinate == replacement.coordinate
        {
            previous_index += 1;
        }
        final_count = final_count
            .checked_add(1)
            .ok_or(PipelineError::ArithmeticOverflow("source scan hint count"))?;
    }
    final_count = final_count
        .checked_add(previous.len().saturating_sub(previous_index))
        .ok_or(PipelineError::ArithmeticOverflow("source scan hint count"))?;

    let mut merged = reserve_retained_vec(final_count, "source scan hints", budget)?;
    previous_index = 0;
    for replacement in replacements {
        while previous_index < previous.len()
            && previous[previous_index].coordinate < replacement.coordinate
        {
            push_cloned_scan_hint(&mut merged, &previous[previous_index], budget)?;
            previous_index += 1;
        }
        if previous_index < previous.len()
            && previous[previous_index].coordinate == replacement.coordinate
        {
            previous_index += 1;
        }
        merged.push(replacement);
    }
    while previous_index < previous.len() {
        push_cloned_scan_hint(&mut merged, &previous[previous_index], budget)?;
        previous_index += 1;
    }
    debug_assert_eq!(merged.len(), final_count);
    Ok(merged)
}

fn push_cloned_scan_hint(
    hints: &mut Vec<SourceScanHint>,
    hint: &SourceScanHint,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    charge_retained_string(&hint.relative_path, "source scan hint path clone", budget)?;
    hints.push(SourceScanHint {
        coordinate: hint.coordinate,
        relative_path: clone_precharged_string(&hint.relative_path, "source scan hint path clone")?,
        source_length: hint.source_length,
        source_modified_unix_ms: hint.source_modified_unix_ms,
        metadata_length: hint.metadata_length,
        metadata_modified_unix_ms: hint.metadata_modified_unix_ms,
    });
    Ok(())
}

fn charge_cached_analysis_clone(
    analysis: &AssetAnalysis,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    let mut encoded = CountingWriter::default();
    serde_json::to_writer(&mut encoded, analysis)?;
    let clone_work = encoded
        .bytes
        .checked_mul(7)
        .and_then(|bytes| bytes.checked_add(4 * 1024))
        .ok_or(PipelineError::ArithmeticOverflow(
            "cached analysis clone work",
        ))?;
    budget.consume_bytes(clone_work)?;
    let entries = analysis
        .references
        .len()
        .saturating_add(analysis.container_entries.len())
        .saturating_add(analysis.diagnostics.len())
        .saturating_add(analysis.truncations.len())
        .saturating_add(analysis.search.hierarchy_paths.len())
        .saturating_add(analysis.search.script_symbols.len())
        .saturating_add(analysis.search.referenced_script_guids.len())
        .saturating_add(analysis.graph_inputs.objects.len())
        .saturating_add(1);
    budget.consume_entries(
        u64::try_from(entries)
            .map_err(|_| PipelineError::ArithmeticOverflow("cached analysis clone entries"))?,
    )?;
    budget.consume_bytes(
        u64::try_from(size_of::<AssetAnalysis>())
            .map_err(|_| PipelineError::ArithmeticOverflow("cached analysis clone header"))?,
    )?;
    Ok(())
}

fn reserve_retained_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, PipelineError> {
    let members =
        u64::try_from(capacity).map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    let bytes = vec_allocation_bytes::<T>(capacity)
        .map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    budget.check_entries(members)?;
    budget.check_members(members)?;
    budget.check_bytes(bytes)?;
    budget.consume_entries(members)?;
    budget.consume_members(members)?;
    budget.consume_bytes(bytes)?;

    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| PipelineError::Allocation {
            resource,
            requested: capacity,
            unit: "elements",
            source,
        })?;
    Ok(values)
}

fn charge_retained_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    let bytes = string_allocation_bytes(value.len())
        .map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn clone_precharged_string(value: &str, resource: &'static str) -> Result<String, PipelineError> {
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|source| PipelineError::Allocation {
            resource,
            requested: value.len(),
            unit: "bytes",
            source,
        })?;
    cloned.push_str(value);
    Ok(cloned)
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let bytes = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("encoded analysis length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("encoded analysis length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn batch_matches_state(batch: &AssetAnalysisBatch, state: &SourceStateSnapshot) -> bool {
    batch.workspace == state.workspace()
        && batch.revision == state.revision()
        && batch.assets.as_slice() == state.assets()
}

fn retain_scan_hints_for_assets(
    scan_hints: &mut Vec<SourceScanHint>,
    sorted_assets: &[AssetAnalysis],
) {
    scan_hints.retain(|hint| {
        sorted_assets
            .binary_search_by_key(&hint.coordinate, |analysis| analysis.source.coordinate)
            .is_ok()
    });
}

fn canonical_transaction_ids(
    receipts: &TransactionReceiptWindow,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<TransactionId>, PipelineError> {
    let capacity = receipts.ids().len();
    let mut transactions = reserve_retained_vec(capacity, "canonical transaction IDs", budget)?;
    transactions.extend(receipts.ids());
    transactions.sort_unstable();
    Ok(transactions)
}

#[cfg(test)]
mod tests {
    use unity_asset::{SourceKind, WorkspaceId};
    use unity_asset_search_core::SearchKind;

    use super::*;
    use crate::analysis::{AnalyzedSource, SearchFacts};

    #[test]
    fn workspace_candidate_retains_scan_hints_only_for_published_assets() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let first = test_analysis(workspace, 1);
        let removed = test_analysis(workspace, 2);
        let third = test_analysis(workspace, 3);
        let expected = [first.source.coordinate, third.source.coordinate];
        let mut hints = vec![
            test_scan_hint(&first),
            test_scan_hint(&removed),
            test_scan_hint(&third),
        ];

        retain_scan_hints_for_assets(&mut hints, &[first, third]);

        assert_eq!(
            hints.iter().map(|hint| hint.coordinate).collect::<Vec<_>>(),
            expected
        );
    }

    fn test_analysis(workspace: WorkspaceId, local: u128) -> AssetAnalysis {
        let source = SourceId::new(workspace, SourceKind::Yaml, local).unwrap();
        let relative_path = format!("Assets/{local}.asset");
        AssetAnalysis::new(
            AnalyzedSource {
                coordinate: IndexedSourceCoordinate::workspace(source),
                relative_path: relative_path.clone(),
                content_digest: DigestV1::hash_bytes(relative_path.as_bytes()),
                length: 1,
                search_kind: SearchKind::Asset,
                guid: None,
                workspace_source: Some(source),
                workspace_fingerprint: None,
                locator: None,
            },
            SearchFacts::default(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
        )
    }

    fn test_scan_hint(analysis: &AssetAnalysis) -> SourceScanHint {
        SourceScanHint {
            coordinate: analysis.source.coordinate,
            relative_path: analysis.source.relative_path.clone(),
            source_length: analysis.source.length,
            source_modified_unix_ms: None,
            metadata_length: None,
            metadata_modified_unix_ms: None,
        }
    }
}
