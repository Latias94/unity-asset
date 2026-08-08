use std::borrow::Cow;
use std::collections::TryReserveError;
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use unity_asset::reference::{ReferenceGraphBuildOptions, ReferenceGraphError};
use unity_asset::workspace::{
    AssetWorkspace, SourceAdmissionBatch, SourceAdmissionBatchAllocationError,
    SourceAdmissionBatchPushError, SourceAdmissionDisposition, SourceAdmissionError,
    SourceAdmissionErrorCategory, SourceAdmissionOperation, SourceAdmissionPolicy,
    SourceOpenRequest, WorkspaceError, WorkspaceLookup, WorkspaceOptions, WorkspaceSource,
    WorkspaceView, recognize_source,
};
use unity_asset::{
    AssetLoadBudget, BudgetError, ChangeSet, ContractError, Diagnostic, DiagnosticError,
    DiagnosticSeverity, DigestV1, ObjectAddress, ObjectId, SourceAlias, SourceId, SourceLocator,
    TransactionId, WorkspaceId, WorkspaceRevision,
};
use unity_asset_core::{string_allocation_bytes, vec_allocation_bytes};
use unity_asset_search_core::{SearchKind, SearchRequest};
use unity_asset_search_protocol::{
    ApiErrorCode, GenerationMaintenanceState, GenerationMaintenanceStatus,
    MAX_REINDEX_PUBLISH_WARNINGS, ReferenceRequest, ReferencesResponse, ReindexEvidence,
    SearchResponse, SuggestResponse,
};

use crate::analysis::{
    AnalysisMetrics, AnalysisTruncation, AnalysisTruncationKind, AssetAnalysis, AssetAnalysisBatch,
    ReferenceDependencyKey, ReferenceResolutionProjection,
};
use crate::analyzer::{AnalysisError, AnalyzerLimits, AssetAnalyzer, WorkspaceAnalysisContext};
use crate::config::{IndexPaths, SearchIndexOptions};
use crate::generation::{
    FilesystemReindexIntent, FilesystemReindexScope, GenerationManifestError,
    GenerationProjectionDigests, GenerationProjectionSummary, GenerationStamp,
    SearchGenerationIdentityV1, SearchGenerationManifestV1,
};
#[cfg(test)]
use crate::generation_store::GenerationFailpoint;
use crate::generation_store::{
    GenerationActivationEvidence, GenerationBuild, GenerationDiskEstimate,
    GenerationPublishWarning, GenerationPublishWarningKind, GenerationRebuildBootstrap,
    GenerationSnapshot, GenerationStore, GenerationStoreError, GenerationStoreOptions,
    SourceScanHint, SourceStateError, SourceStateSnapshot, TransactionReceiptMembership,
    TransactionReceiptWindow,
};
use crate::path_semantics::{ProjectPath, ProjectPathError, compare_portable_paths};
use crate::projection::{
    GenerationProjection, ProjectionCategory, ProjectionError, ProjectionLimits, ProjectionMetrics,
    project_batch,
};
use crate::query::{QueryEngine, QuerySnapshot, SearchQueryFields};
use crate::reference_query::{
    ReferenceQueryCompleteness, ReferenceQueryCompletenessError, ReferenceQueryEngine,
    ReferenceQueryError, ReferenceQuerySnapshot,
};
use crate::scan::{
    FileHint, PathRejection, ProjectScanner, ProjectSourcePath, ReadSource, ScanDiagnostic,
    ScanError, ScanIntent, ScanMetrics, ScanMode, ScanValidation, SourceHints, SourcePart,
};
use crate::semantics::{AnalysisCacheIdentityV1, SearchSemantics};
use crate::source_coordinate::IndexedSourceCoordinate;
use crate::store::{ProjectionReaders, ProjectionStore, is_rebuildable_projection_schema_version};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipelineBuildDisposition {
    Published,
    NoChange,
    AlreadyApplied,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PipelineBuildMetrics {
    pub(crate) scan: ScanMetrics,
    pub(crate) analysis: AnalysisMetrics,
    pub(crate) projection: ProjectionMetrics,
    pub(crate) reused_assets: u64,
    pub(crate) graph_refreshed_assets: u64,
    pub(crate) dependency_candidate_assets: u64,
    pub(crate) dependency_closure_assets: u64,
    pub(crate) workspace_parse_failures: u64,
    pub(crate) scan_diagnostics: u64,
    pub(crate) forced_full_scan: bool,
    pub(crate) forced_full_analysis: bool,
    pub(crate) full_dependency_scan: bool,
}

#[derive(Clone)]
pub(crate) struct ActiveGeneration {
    snapshot: GenerationSnapshot,
    stamp: GenerationStamp,
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

    fn set_desired_revision(&mut self, revision: WorkspaceRevision) {
        self.stamp = self.stamp.clone().with_desired_revision(revision);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PipelineBuildOutput {
    pub(crate) disposition: PipelineBuildDisposition,
    pub(crate) active: Option<Arc<ActiveGeneration>>,
    pub(crate) metrics: PipelineBuildMetrics,
    pub(crate) disk_estimate: Option<GenerationDiskEstimate>,
    pub(crate) warnings: Vec<String>,
    pub(crate) transaction: Option<TransactionId>,
    pub(crate) target_revision: Option<WorkspaceRevision>,
    pub(crate) duration_ms: u128,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanValidationCheckpoint {
    NoChangePreReturn,
    ActivationPreCommit,
}

#[cfg(test)]
struct ScanValidationHook {
    checkpoint: ScanValidationCheckpoint,
    action: Box<dyn FnOnce() + Send + 'static>,
}

pub(crate) struct SearchGenerationPipeline {
    paths: IndexPaths,
    options: SearchIndexOptions,
    options_digest: DigestV1,
    semantics: SearchSemantics,
    analysis_cache_identity: AnalysisCacheIdentityV1,
    scanner: ProjectScanner,
    workspace: AssetWorkspace,
    workspace_roots: WorkspaceRoots,
    workspace_hydrated: bool,
    active_analysis_match: bool,
    active_compatibility_match: bool,
    rebuild_bootstrap: Option<GenerationRebuildBootstrap>,
    store: GenerationStore,
    source_state: Option<SourceStateSnapshot>,
    active: Option<Arc<ActiveGeneration>>,
    generation_maintenance: GenerationMaintenanceStatus,
    pending_publish_warnings: Vec<String>,
    pending_publish_warnings_omitted: bool,
    #[cfg(test)]
    publish_failpoint: Option<GenerationFailpoint>,
    #[cfg(test)]
    scan_validation_hook: Option<ScanValidationHook>,
}

impl fmt::Debug for SearchGenerationPipeline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchGenerationPipeline")
            .field("paths", &self.paths)
            .field("options", &self.options)
            .field("workspace", &self.workspace)
            .field("workspace_hydrated", &self.workspace_hydrated)
            .field("active_analysis_match", &self.active_analysis_match)
            .field(
                "active_compatibility_match",
                &self.active_compatibility_match,
            )
            .field(
                "active_generation",
                &self.active.as_ref().map(|active| active.stamp()),
            )
            .finish_non_exhaustive()
    }
}

impl SearchGenerationPipeline {
    pub(crate) fn open(
        paths: IndexPaths,
        options: SearchIndexOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PipelineError> {
        Self::open_with_semantics(paths, options, SearchSemantics::current(), budget)
    }

    fn open_with_semantics(
        paths: IndexPaths,
        options: SearchIndexOptions,
        semantics: SearchSemantics,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PipelineError> {
        Self::open_configured(
            paths,
            options,
            semantics,
            budget,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn open_with_startup_recovery_failpoint(
        paths: IndexPaths,
        options: SearchIndexOptions,
        failpoint: GenerationFailpoint,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PipelineError> {
        Self::open_configured(
            paths,
            options,
            SearchSemantics::current(),
            budget,
            Some(failpoint),
        )
    }

    fn open_configured(
        paths: IndexPaths,
        options: SearchIndexOptions,
        semantics: SearchSemantics,
        budget: &mut AssetLoadBudget,
        #[cfg(test)] startup_recovery_failpoint: Option<GenerationFailpoint>,
    ) -> Result<Self, PipelineError> {
        let options = options.validate().map_err(PipelineError::Configuration)?;
        let options_digest = paths
            .logical_configuration_digest(options)
            .map_err(PipelineError::Configuration)?;
        let analysis_cache_identity = semantics
            .analysis_cache_identity(options_digest)
            .map_err(|error| PipelineError::Configuration(anyhow::Error::new(error)))?;
        let scanner = ProjectScanner::new(&paths, options, options.scan_limits())
            .map_err(PipelineError::Configuration)?;
        let store_options = GenerationStoreOptions {
            retain_previous_generations: options.retain_previous_generations,
        };
        #[cfg(not(test))]
        let opened = GenerationStore::open_private(
            paths.private_index_root().clone(),
            store_options,
            budget,
        )?;
        #[cfg(test)]
        let opened = match startup_recovery_failpoint {
            Some(failpoint) => GenerationStore::open_private_with_startup_recovery_failpoint(
                paths.private_index_root().clone(),
                store_options,
                budget,
                failpoint,
            )?,
            None => GenerationStore::open_private(
                paths.private_index_root().clone(),
                store_options,
                budget,
            )?,
        };
        let (store, staging_recovery, startup_disposition) = opened.into_parts();
        let rebuild_bootstrap = startup_disposition
            .rebuild_required()
            .map(|required| required.bootstrap().clone());
        let generation_maintenance = match staging_recovery {
            Ok(report) => GenerationMaintenanceStatus {
                state: GenerationMaintenanceState::Clean,
                last_recovered_entries: report.removed_entries(),
                last_cleanup_failure: None,
            },
            Err(error) => GenerationMaintenanceStatus {
                state: GenerationMaintenanceState::RecoveryRequired,
                last_recovered_entries: 0,
                last_cleanup_failure: Some(crate::wire::bounded_error_message(error.to_string())),
            },
        };
        scanner
            .validate_project_root_binding()
            .map_err(|error| PipelineError::Scan(Box::new(error)))?;
        let recovered = store.active().cloned();
        let (active_semantics_match, active_configuration_match, active_source_state_layout_match) =
            recovered.as_ref().map_or((true, true, true), |snapshot| {
                let persisted_semantics = snapshot.manifest().semantics();
                let configuration_match = snapshot.manifest().options_digest() == options_digest;
                (
                    persisted_semantics == semantics,
                    configuration_match,
                    persisted_semantics.source_state_layout_compatible_with(semantics),
                )
            });
        // A semantic mismatch can include an incompatible source-state wire shape. The manifest
        // remains sufficient to preserve the workspace identity, but no persisted analysis or
        // projection is decoded under rules that did not create it.
        let source_state = if active_source_state_layout_match {
            recovered
                .as_ref()
                .map(|snapshot| snapshot.load_source_state(budget))
                .transpose()?
        } else {
            None
        };
        if let Some(source_state) = source_state.as_ref() {
            source_state.validate_project_path_space(paths.project_path_space())?;
        }
        let workspace = source_state
            .as_ref()
            .map(SourceStateSnapshot::workspace)
            .or_else(|| {
                recovered
                    .as_ref()
                    .map(|snapshot| snapshot.manifest().workspace())
            })
            .or_else(|| {
                rebuild_bootstrap
                    .as_ref()
                    .map(GenerationRebuildBootstrap::workspace)
            })
            .map_or_else(AssetWorkspace::new, |workspace| {
                AssetWorkspace::with_workspace_id(workspace, WorkspaceOptions::lenient())
            })?;
        let active_analysis_match = match (recovered.as_ref(), source_state.as_ref()) {
            (Some(_), Some(state)) => state.analysis_cache_identity() == analysis_cache_identity,
            (Some(_), None) => false,
            (None, None) => true,
            (None, Some(_)) => {
                return Err(PipelineError::Invariant(
                    "source state exists without an activated generation",
                ));
            }
        };
        let mut active_compatibility_match = if recovered.is_some() {
            active_semantics_match && active_configuration_match
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
                        options,
                        active_semantics_match,
                        active_configuration_match,
                        budget,
                    )?)),
                    Err(error) if is_rebuildable_projection_schema_version(&error) => {
                        // The source-state snapshot remains authoritative enough to preserve the
                        // workspace identity and incrementality baseline. Only this derived
                        // projection layout is obsolete, so force the next request through a
                        // complete current-format rebuild instead of treating recovery as a
                        // corrupt store.
                        active_compatibility_match = false;
                        None
                    }
                    Err(error) => return Err(PipelineError::Projection(error)),
                }
            }
            (Some(_), None) if !active_source_state_layout_match => None,
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
        Ok(Self {
            paths,
            options,
            options_digest,
            semantics,
            analysis_cache_identity,
            scanner,
            workspace,
            workspace_roots: WorkspaceRoots::default(),
            workspace_hydrated: false,
            active_analysis_match,
            active_compatibility_match,
            rebuild_bootstrap,
            store,
            source_state,
            active,
            generation_maintenance,
            pending_publish_warnings: Vec::new(),
            pending_publish_warnings_omitted: false,
            #[cfg(test)]
            publish_failpoint: None,
            #[cfg(test)]
            scan_validation_hook: None,
        })
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
        self.scan_validation_hook = Some(ScanValidationHook {
            checkpoint,
            action: Box::new(action),
        });
    }

    #[cfg(test)]
    fn run_scan_validation_hook(
        hook: &mut Option<ScanValidationHook>,
        checkpoint: ScanValidationCheckpoint,
    ) {
        let should_run = hook
            .as_ref()
            .is_some_and(|hook| hook.checkpoint == checkpoint);
        if should_run {
            let hook = hook
                .take()
                .expect("scan validation hook disappeared after matching its checkpoint");
            (hook.action)();
        }
    }

    pub(crate) fn active(&self) -> Option<Arc<ActiveGeneration>> {
        self.active.clone()
    }

    pub(crate) fn generation_maintenance(&self) -> GenerationMaintenanceStatus {
        self.generation_maintenance.clone()
    }

    pub(crate) fn reindex_filesystem(
        &mut self,
        intent: FilesystemReindexIntent,
        budget: &mut AssetLoadBudget,
    ) -> Result<PipelineBuildOutput, PipelineError> {
        let started = Instant::now();
        let reconcile_staging = matches!(&intent.scope, FilesystemReindexScope::Reconcile);
        let requested = match intent.scope {
            FilesystemReindexScope::Full => ScanIntent::Full,
            FilesystemReindexScope::Reconcile => ScanIntent::Reconcile,
            FilesystemReindexScope::ChangedPaths { paths } => ScanIntent::ChangedPaths(paths),
        };
        if reconcile_staging {
            let mut cleanup_budget = AssetLoadBudget::default();
            match self.store.reconcile_abandoned_staging(&mut cleanup_budget) {
                Ok(report) => self.record_generation_recovery(report.removed_entries()),
                Err(error) => {
                    self.record_generation_cleanup_failure(error.to_string());
                    return Err(PipelineError::Store(Box::new(error)));
                }
            }
        }
        let discard_cached_analysis = !self.active_analysis_match;
        let force_full = !self.workspace_hydrated || !self.active_compatibility_match;
        let scan_intent = if force_full {
            ScanIntent::Full
        } else {
            requested
        };
        let prepared = self.prepare_filesystem_batch(
            scan_intent,
            force_full,
            discard_cached_analysis,
            budget,
        )?;
        self.publish_batch(prepared, None, None, started, budget)
    }

    pub(crate) fn reindex_workspace(
        &mut self,
        changes: ChangeSet,
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<PipelineBuildOutput, PipelineError> {
        let started = Instant::now();
        if !self.active_compatibility_match {
            return Err(PipelineError::FilesystemReindexRequired);
        }
        let indexed = self
            .source_state
            .as_ref()
            .map(|state| (state.workspace(), state.revision()));
        if let Some((workspace, revision)) = indexed {
            if workspace != changes.workspace() {
                return Err(PipelineError::WorkspaceMismatch {
                    expected: workspace,
                    actual: changes.workspace(),
                });
            }
            let receipts = self
                .store
                .active()
                .ok_or(PipelineError::Invariant(
                    "active generation disappeared during receipt lookup",
                ))?
                .transaction_receipts();
            match receipts.membership(&changes, budget)? {
                TransactionReceiptMembership::Exact => {
                    return Ok(PipelineBuildOutput {
                        disposition: PipelineBuildDisposition::AlreadyApplied,
                        active: self.active.clone(),
                        metrics: PipelineBuildMetrics::default(),
                        disk_estimate: None,
                        warnings: Vec::new(),
                        transaction: Some(changes.transaction()),
                        target_revision: Some(changes.to_revision()),
                        duration_ms: started.elapsed().as_millis(),
                    });
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
                validate_change_set_view(&changes, view)?;
                let prepared = self.prepare_observed_transaction_batch(&changes, budget)?;
                return self.publish_batch(
                    prepared,
                    Some(changes.transaction()),
                    Some(changes.to_revision()),
                    started,
                    budget,
                );
            }
            self.mark_desired_revision(changes.to_revision(), budget)?;
            validate_change_set_view(&changes, view)?;
        } else {
            validate_change_set_view(&changes, view)?;
        }

        let prepared = self.prepare_workspace_batch(&changes, view, budget)?;
        self.publish_batch(
            prepared,
            Some(changes.transaction()),
            Some(changes.to_revision()),
            started,
            budget,
        )
    }

    fn prepare_observed_transaction_batch(
        &self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedBatch, PipelineError> {
        let state = self.source_state.as_ref().ok_or(PipelineError::Invariant(
            "observed transaction requires indexed source state",
        ))?;
        if state.workspace() != changes.workspace() || state.revision() != changes.to_revision() {
            return Err(PipelineError::Invariant(
                "observed transaction does not match indexed workspace revision",
            ));
        }

        let transaction_receipts = self
            .store
            .active()
            .ok_or(PipelineError::Invariant(
                "active generation disappeared during reconciled receipt update",
            ))?
            .transaction_receipts()
            .after_reconciled_target(state.workspace(), state.revision(), changes, budget)?;

        let mut assets = reserve_retained_vec(
            state.assets().len(),
            "observed transaction cached assets",
            budget,
        )?;
        for analysis in state.assets() {
            charge_cached_analysis_clone(analysis, budget)?;
            assets.push(analysis.clone());
        }

        let mut scan_hints = reserve_retained_vec(
            state.scan_hints().len(),
            "observed transaction scan hints",
            budget,
        )?;
        for hint in state.scan_hints() {
            push_cloned_scan_hint(&mut scan_hints, hint, budget)?;
        }

        let transactions = canonical_transaction_ids(&transaction_receipts, budget)?;
        let batch = AssetAnalysisBatch::new(
            state.workspace(),
            state.revision(),
            transactions,
            assets,
            AnalysisMetrics::default(),
        );
        Ok(PreparedBatch {
            batch,
            scan_hints,
            metrics: PipelineBuildMetrics {
                reused_assets: saturating_usize_to_u64(state.assets().len()),
                ..PipelineBuildMetrics::default()
            },
            transaction_receipts,
            workspace: None,
            filesystem_validation: None,
        })
    }

    fn prepare_filesystem_batch(
        &mut self,
        intent: ScanIntent,
        forced_full_scan: bool,
        discard_cached_analysis: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedBatch, PipelineError> {
        let mut cached = if discard_cached_analysis {
            CachedAssets::empty()
        } else {
            self.clone_previous_assets(budget)?
        };
        let mut workspace = self.workspace.fork_candidate();
        let known_paths = clone_known_paths(self.source_state.as_ref(), budget)?;
        let mut plan = self.scanner.plan(intent, &known_paths, budget).map_err(
            |error| match error {
                ScanError::Budget(error) => PipelineError::Budget(error),
                ScanError::ChangedPathProjectMismatch { expected, actual } => {
                    PipelineError::Configuration(anyhow::anyhow!(
                        "changed paths belong to project {actual}, but this index owns {expected}"
                    ))
                }
                error => PipelineError::Scan(Box::new(error)),
            },
        )?;
        let invalid_changed_path = matches!(plan.mode, ScanMode::ChangedPaths)
            && plan.diagnostics.iter().any(|diagnostic| {
                matches!(
                    diagnostic,
                    ScanDiagnostic::PathRejected {
                        reason: PathRejection::InvalidPath
                            | PathRejection::Symlink
                            | PathRejection::UnsupportedFileType
                            | PathRejection::NonUtf8RelativePath
                            | PathRejection::UnsupportedCaseSensitiveDirectory,
                        ..
                    }
                )
            });
        if invalid_changed_path {
            return Err(PipelineError::ScanRequestRejected {
                diagnostics: plan.diagnostics,
            });
        }
        let scan_plan_failed = plan.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic,
                ScanDiagnostic::WalkFailed { .. }
                    | ScanDiagnostic::ReadFailed { .. }
                    | ScanDiagnostic::PathRejected {
                        reason: PathRejection::UnsupportedCaseSensitiveDirectory,
                        ..
                    }
            )
        });
        if scan_plan_failed {
            return Err(PipelineError::ScanPlanRejected {
                diagnostics: plan.diagnostics,
            });
        }

        let mut metrics = PipelineBuildMetrics {
            scan: plan.metrics.clone(),
            scan_diagnostics: saturating_usize_to_u64(plan.diagnostics.len()),
            forced_full_scan,
            ..PipelineBuildMetrics::default()
        };
        let mut read_sources =
            reserve_retained_vec(plan.present.len(), "read source list", budget)?;
        for candidate_index in 0..plan.present.len() {
            let outcome = {
                let candidate = &plan.present[candidate_index];
                let coordinate = candidate.coordinate();
                let previous_identity = cached
                    .get(coordinate)
                    .map(|analysis| analysis.source.content_digest);
                self.scanner
                    .read_source(candidate, previous_identity, budget)
            };
            metrics.scan.merge(&outcome.metrics);
            metrics.scan_diagnostics = metrics
                .scan_diagnostics
                .saturating_add(saturating_usize_to_u64(outcome.diagnostics.len()));
            let accepted = match outcome.accepted {
                Some(accepted) => accepted,
                None => {
                    return Err(PipelineError::SourceReadRejected {
                        relative_path: plan.present[candidate_index].relative_path().to_owned(),
                        diagnostics: outcome.diagnostics,
                    });
                }
            };
            plan.record_source_proof(accepted.proof, budget)
                .map_err(|error| match error {
                    ScanError::Budget(error) => PipelineError::Budget(error),
                    error => PipelineError::Scan(Box::new(error)),
                })?;
            read_sources.push(ScannedSource {
                source: accepted.source,
                diagnostics: outcome.diagnostics,
            });
        }
        read_sources.sort_unstable_by_key(|scanned| scanned.source.coordinate);
        if read_sources
            .windows(2)
            .any(|pair| pair[0].source.coordinate == pair[1].source.coordinate)
        {
            return Err(PipelineError::Invariant(
                "scanner returned duplicate source coordinates",
            ));
        }

        let changed_read_sources = read_sources
            .iter()
            .filter(|scanned| {
                let source = &scanned.source;
                !source.unchanged
                    || cached
                        .get(source.coordinate)
                        .is_none_or(|analysis| analysis.source.relative_path != source.rel_path)
            })
            .count();
        let changed_source_capacity = plan.deleted.len().checked_add(changed_read_sources).ok_or(
            PipelineError::ArithmeticOverflow("changed source coordinate list"),
        )?;
        let mut changed_sources = reserve_retained_vec(
            changed_source_capacity,
            "changed source coordinate list",
            budget,
        )?;
        changed_sources.extend(plan.deleted.iter().map(ProjectSourcePath::coordinate));
        for scanned in &read_sources {
            let source = &scanned.source;
            if !source.unchanged
                || cached
                    .get(source.coordinate)
                    .is_none_or(|analysis| analysis.source.relative_path != source.rel_path)
            {
                changed_sources.push(source.coordinate);
            }
        }
        changed_sources.sort_unstable();
        changed_sources.dedup();
        let mut impact = DependencyImpact::default();
        for coordinate in &changed_sources {
            if let Some(analysis) = cached.get(*coordinate) {
                impact.add_analysis_identity(analysis, budget)?;
                impact.add_source_path(&analysis.source.relative_path, budget)?;
            }
        }
        for scanned in &read_sources {
            let source = &scanned.source;
            if changed_sources.binary_search(&source.coordinate).is_ok() {
                impact.add_guid(source.guid.as_deref(), budget)?;
                impact.add_source_path(source.rel_path.as_str(), budget)?;
            }
        }
        let changed_evidence_incomplete = changed_sources.iter().any(|coordinate| {
            cached.get(*coordinate).is_some_and(|analysis| {
                analysis.source.workspace_source.is_some() && !analysis.graph_inputs.complete
            })
        });

        let was_hydrated = self.workspace_hydrated;
        let scan_hints = self.merge_scan_hints(&plan.deleted, &read_sources, budget)?;
        let reload_count = read_sources
            .iter()
            .filter(|scanned| {
                let source = &scanned.source;
                let root_is_loaded = self.workspace_roots.contains_key(source.coordinate);
                let spelling_changed = cached
                    .get(source.coordinate)
                    .is_some_and(|analysis| analysis.source.relative_path != source.rel_path);
                !was_hydrated || !source.unchanged || spelling_changed || !root_is_loaded
            })
            .count();
        let mut reloads =
            reserve_retained_vec(reload_count, "filesystem source reload plans", budget)?;
        for (source_index, scanned) in read_sources.iter().enumerate() {
            let source = &scanned.source;
            let existing_root = self.workspace_roots.get(source.coordinate).copied();
            let spelling_changed = cached
                .get(source.coordinate)
                .is_some_and(|analysis| analysis.source.relative_path != source.rel_path);
            let needs_reload =
                !was_hydrated || !source.unchanged || spelling_changed || existing_root.is_none();
            if !needs_reload {
                continue;
            }
            let load = source.bytes.is_some() && is_workspace_candidate(source, budget)?;
            reloads.push(FilesystemReloadPlan {
                source_index,
                existing_root,
                load,
            });
        }
        let deleted_unloads = plan
            .deleted
            .iter()
            .filter(|path| self.workspace_roots.contains_key(path.coordinate()))
            .count();
        let reload_unloads = reloads
            .iter()
            .filter(|reload| reload.existing_root.is_some())
            .count();
        let reload_loads = reloads.iter().filter(|reload| reload.load).count();
        let admission_operation_count = deleted_unloads
            .checked_add(reload_unloads)
            .and_then(|count| count.checked_add(reload_loads))
            .ok_or(PipelineError::ArithmeticOverflow(
                "filesystem source admission operation count",
            ))?;
        let mut admission_batch =
            SourceAdmissionBatch::with_capacity(admission_operation_count, budget)
                .map_err(PipelineError::from)?;
        let mut admission_effects = reserve_retained_vec(
            admission_operation_count,
            "filesystem source admission effects",
            budget,
        )?;
        let root_update_count = plan.deleted.len().checked_add(reloads.len()).ok_or(
            PipelineError::ArithmeticOverflow("filesystem workspace root update count"),
        )?;
        let mut root_updates = reserve_retained_vec(
            root_update_count,
            "filesystem workspace root updates",
            budget,
        )?;
        for (deleted_index, deleted) in plan.deleted.iter().enumerate() {
            root_updates.push(FilesystemRootUpdate::Delete { deleted_index });
            if let Some(root) = self.workspace_roots.get(deleted.coordinate()).copied() {
                admission_batch
                    .try_push(SourceAdmissionOperation::Unload(root), budget)
                    .map_err(PipelineError::from)?;
                admission_effects.push(FilesystemAdmissionEffect::Unload { source_id: root });
            }
        }

        let mut parse_failures =
            reserve_retained_vec(read_sources.len(), "workspace parse failures", budget)?;
        parse_failures.resize_with(read_sources.len(), || None::<String>);
        for reload in reloads {
            let source_index = reload.source_index;
            let source = &read_sources[source_index].source;
            let root_update_index = root_updates.len();
            root_updates.push(FilesystemRootUpdate::Source {
                source_index,
                replacement: None,
            });
            if let Some(root) = reload.existing_root {
                admission_batch
                    .try_push(SourceAdmissionOperation::Unload(root), budget)
                    .map_err(PipelineError::from)?;
                admission_effects.push(FilesystemAdmissionEffect::Unload { source_id: root });
            }
            if !reload.load {
                continue;
            }
            let bytes = source.bytes.as_ref().ok_or(PipelineError::Invariant(
                "planned workspace source load has no retained bytes",
            ))?;
            let alias = SourceAlias::new(clone_checked_string(
                &source.rel_path,
                "workspace source alias",
                budget,
            )?)?;
            let request = SourceOpenRequest::new(
                clone_checked_path(&source.abs_path, "workspace source path", budget)?,
                alias,
            );
            admission_batch
                .try_push(
                    SourceAdmissionOperation::LoadBudgetedBytes {
                        request,
                        image: bytes.clone(),
                    },
                    budget,
                )
                .map_err(PipelineError::from)?;
            admission_effects.push(FilesystemAdmissionEffect::Load {
                source_index,
                root_update_index,
            });
        }

        let report = workspace
            .admit_sources(
                admission_batch,
                SourceAdmissionPolicy::TolerantContent,
                budget,
            )
            .map_err(PipelineError::from)?;
        let outcomes = report.into_outcomes();
        if outcomes.len() != admission_effects.len() {
            return Err(PipelineError::Invariant(
                "source admission outcome count differs from its operation count",
            ));
        }
        for (ordinal, (outcome, effect)) in outcomes.into_iter().zip(admission_effects).enumerate()
        {
            let ordinal = u64::try_from(ordinal).map_err(|_| {
                PipelineError::ArithmeticOverflow("filesystem source admission ordinal")
            })?;
            if outcome.operation_ordinal() != ordinal {
                return Err(PipelineError::Invariant(
                    "source admission outcomes are not in operation order",
                ));
            }
            match (effect, outcome.into_disposition()) {
                (
                    FilesystemAdmissionEffect::Unload {
                        source_id: expected,
                    },
                    SourceAdmissionDisposition::Unloaded { source_id: actual },
                ) if expected == actual => {}
                (
                    FilesystemAdmissionEffect::Load {
                        root_update_index, ..
                    },
                    SourceAdmissionDisposition::Loaded { source_id }
                    | SourceAdmissionDisposition::Unchanged { source_id },
                ) => {
                    root_updates
                        .get_mut(root_update_index)
                        .ok_or(PipelineError::Invariant(
                            "source admission references an unknown workspace root update",
                        ))?
                        .set_replacement(source_id)?;
                }
                (
                    FilesystemAdmissionEffect::Load { source_index, .. },
                    SourceAdmissionDisposition::Rejected(rejection),
                ) => {
                    metrics.workspace_parse_failures =
                        metrics.workspace_parse_failures.saturating_add(1);
                    parse_failures[source_index] = Some(retained_display_string(
                        rejection.failure(),
                        "workspace parse failure",
                        budget,
                    )?);
                }
                _ => {
                    return Err(PipelineError::Invariant(
                        "source admission outcome does not match its filesystem operation",
                    ));
                }
            }
        }
        root_updates.sort_unstable_by(|left, right| {
            left.coordinate(&plan.deleted, &read_sources)
                .cmp(&right.coordinate(&plan.deleted, &read_sources))
        });
        if root_updates.windows(2).any(|pair| {
            pair[0].coordinate(&plan.deleted, &read_sources)
                == pair[1].coordinate(&plan.deleted, &read_sources)
        }) {
            return Err(PipelineError::Invariant(
                "filesystem scan produced duplicate workspace root updates",
            ));
        }
        let workspace_roots = merge_workspace_roots(
            &self.workspace_roots,
            &root_updates,
            &plan.deleted,
            &read_sources,
            budget,
        )?;
        let workspace_hydrated =
            was_hydrated || matches!(plan.mode, ScanMode::Full | ScanMode::Reconcile);
        let observed_revision = workspace.revision();
        let actual_revision_unchanged = self
            .store
            .active()
            .is_some_and(|active| active.manifest().revision() == observed_revision);
        if !actual_revision_unchanged {
            self.mark_desired_revision(observed_revision, budget)?;
        }

        let snapshot = workspace.snapshot();
        let graph = snapshot.reference_graph(ReferenceGraphBuildOptions::unbounded(), budget)?;
        let context = WorkspaceAnalysisContext::build(&snapshot, &graph, budget)?;
        let mut current_sources = snapshot.sources(budget)?;
        current_sources.sort_unstable_by_key(|source| source.id());
        let changed_root_count = changed_sources
            .iter()
            .filter_map(|coordinate| workspace_roots.get(*coordinate).copied())
            .count();
        let mut changed_roots =
            reserve_retained_vec(changed_root_count, "changed workspace roots", budget)?;
        changed_roots.extend(
            changed_sources
                .iter()
                .filter_map(|coordinate| workspace_roots.get(*coordinate).copied()),
        );
        changed_roots.sort_unstable();
        changed_roots.dedup();
        for root in &changed_roots {
            let source = workspace_source(&current_sources, *root)
                .ok_or(PipelineError::UnknownWorkspaceSource(*root))?;
            impact.add_source(source.locator(), budget)?;
        }
        add_current_source_impacts(&mut impact, &current_sources, &changed_roots, budget)?;
        add_current_object_impacts(
            &mut impact,
            &graph,
            &current_sources,
            &changed_roots,
            budget,
        )?;
        let force_full_analysis = discard_cached_analysis
            || (!changed_sources.is_empty()
                && (!graph.is_complete() || changed_evidence_incomplete));
        let dependency_candidate_assets = cached.len();
        metrics.forced_full_analysis = force_full_analysis;
        metrics.full_dependency_scan =
            !changed_sources.is_empty() && dependency_candidate_assets != 0;
        metrics.dependency_candidate_assets = saturating_usize_to_u64(dependency_candidate_assets);
        let affected_source_count = cached
            .iter()
            .filter(|(coordinate, analysis)| {
                if changed_sources.binary_search(coordinate).is_ok()
                    || analysis.source.workspace_source.is_none()
                {
                    return false;
                }
                force_full_analysis || !analysis.complete || impact.matches_analysis(analysis)
            })
            .count();
        let mut affected_sources =
            reserve_retained_vec(affected_source_count, "affected source coordinates", budget)?;
        for (coordinate, analysis) in cached.iter() {
            if changed_sources.binary_search(&coordinate).is_err()
                && analysis.source.workspace_source.is_some()
                && (force_full_analysis || !analysis.complete || impact.matches_analysis(analysis))
            {
                affected_sources.push(coordinate);
            }
        }
        affected_sources.sort_unstable();
        affected_sources.dedup();
        drop(impact);
        for deleted in &plan.deleted {
            cached.remove(deleted.coordinate());
        }
        metrics.dependency_closure_assets = saturating_usize_to_u64(affected_sources.len());
        let analyzer = AssetAnalyzer::new(AnalyzerLimits::default());
        let replaced_asset_count = read_sources
            .iter()
            .filter(|scanned| cached.contains_key(scanned.source.coordinate))
            .count();
        let retained_asset_count = cached.len().checked_sub(replaced_asset_count).ok_or(
            PipelineError::ArithmeticOverflow("filesystem retained asset count"),
        )?;
        let final_asset_count = read_sources
            .len()
            .checked_add(retained_asset_count)
            .ok_or(PipelineError::ArithmeticOverflow("filesystem asset count"))?;
        let mut assets =
            reserve_retained_vec(final_asset_count, "filesystem analysis batch", budget)?;

        for (source_index, scanned) in read_sources.iter().enumerate() {
            let source = &scanned.source;
            let cached_analysis = cached.take(source.coordinate);
            let workspace_root = workspace_roots.get(source.coordinate).copied();
            let workspace_input = workspace_root.map(|root| context.asset(root)).transpose()?;
            let analyzed = match cached_analysis {
                Some(mut cached_analysis)
                    if source.unchanged
                        && cached_analysis.source.relative_path == source.rel_path =>
                {
                    let cached_root_matches =
                        cached_analysis.source.workspace_source == workspace_root;
                    if cached_root_matches && cached_analysis.complete && workspace_input.is_none()
                    {
                        apply_source_scan_diagnostics(
                            &mut cached_analysis,
                            &scanned.diagnostics,
                            &mut metrics.analysis,
                            budget,
                        )?;
                        metrics.reused_assets = metrics.reused_assets.saturating_add(1);
                        assets.push(cached_analysis);
                        continue;
                    }
                    if cached_root_matches
                        && cached_analysis.complete
                        && workspace_input.is_some()
                        && affected_sources.binary_search(&source.coordinate).is_err()
                    {
                        apply_source_scan_diagnostics(
                            &mut cached_analysis,
                            &scanned.diagnostics,
                            &mut metrics.analysis,
                            budget,
                        )?;
                        metrics.reused_assets = metrics.reused_assets.saturating_add(1);
                        assets.push(cached_analysis);
                        continue;
                    }
                    if cached_root_matches
                        && cached_analysis.complete
                        && cached_analysis.graph_inputs.complete
                        && let Some(input) = workspace_input
                    {
                        metrics.graph_refreshed_assets =
                            metrics.graph_refreshed_assets.saturating_add(1);
                        analyzer.refresh_graph_facts(&cached_analysis, input, budget)?
                    } else {
                        analyzer.analyze(source, workspace_input, budget)?
                    }
                }
                _ => analyzer.analyze(source, workspace_input, budget)?,
            };
            let mut analysis = analyzed.analysis;
            apply_source_scan_diagnostics(
                &mut analysis,
                &scanned.diagnostics,
                &mut metrics.analysis,
                budget,
            )?;
            if let Some(message) = parse_failures[source_index].as_deref() {
                mark_workspace_parse_failure(&mut analysis, message, budget)?;
                metrics.analysis.truncations_emitted =
                    metrics.analysis.truncations_emitted.saturating_add(1);
                metrics.analysis.diagnostics_emitted =
                    metrics.analysis.diagnostics_emitted.saturating_add(1);
            }
            metrics.analysis.merge(&analyzed.metrics);
            assets.push(analysis);
        }

        for (coordinate, cached_analysis) in cached.into_remaining() {
            if cached_analysis.source.workspace_source.is_none()
                || affected_sources.binary_search(&coordinate).is_err()
            {
                metrics.reused_assets = metrics.reused_assets.saturating_add(1);
                assets.push(cached_analysis);
                continue;
            }
            let input = context.asset_for_analysis(&cached_analysis)?;
            let analyzed = if cached_analysis.graph_inputs.complete {
                metrics.graph_refreshed_assets = metrics.graph_refreshed_assets.saturating_add(1);
                analyzer.refresh_graph_facts(&cached_analysis, input, budget)?
            } else {
                let source =
                    cached_workspace_read_source(&self.paths, &snapshot, &cached_analysis, budget)?;
                analyzer.analyze(&source, Some(input), budget)?
            };
            metrics.analysis.merge(&analyzed.metrics);
            assets.push(analyzed.analysis);
        }
        metrics.analysis.source_opens = metrics.scan.opened;
        metrics.analysis.source_bytes_read = metrics.scan.read_bytes;

        let transaction_receipts = match self.store.active() {
            Some(active) if active.manifest().workspace() == snapshot.workspace_id() => {
                active.transaction_receipts().clone()
            }
            None if self
                .rebuild_bootstrap
                .as_ref()
                .is_some_and(|bootstrap| bootstrap.workspace() == snapshot.workspace_id()) =>
            {
                self.rebuild_bootstrap
                    .as_ref()
                    .expect("matching rebuild bootstrap disappeared")
                    .transaction_receipts()
                    .clone()
            }
            Some(_) | None => TransactionReceiptWindow::empty(),
        };
        let transactions = canonical_transaction_ids(&transaction_receipts, budget)?;
        let batch = AssetAnalysisBatch::new(
            snapshot.workspace_id(),
            snapshot.revision(),
            transactions,
            assets,
            metrics.analysis,
        );
        let filesystem_validation = plan.into_validation();
        Ok(PreparedBatch {
            batch,
            scan_hints,
            metrics,
            transaction_receipts,
            workspace: Some(PreparedWorkspace {
                workspace,
                roots: workspace_roots,
                hydrated: workspace_hydrated,
            }),
            filesystem_validation: Some(filesystem_validation),
        })
    }

    fn prepare_workspace_batch(
        &self,
        changes: &ChangeSet,
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedBatch, PipelineError> {
        let transaction_receipts = match (self.source_state.as_ref(), self.store.active()) {
            (Some(state), Some(active)) => active.transaction_receipts().after_change_set(
                state.workspace(),
                state.revision(),
                changes,
                budget,
            )?,
            (None, None) => TransactionReceiptWindow::from_change_set(changes, budget)?,
            _ => {
                return Err(PipelineError::Invariant(
                    "generation and source-state lifecycle diverged during receipt update",
                ));
            }
        };
        let mut cached = self.clone_previous_assets(budget)?;
        let mut sources = view.sources(budget)?;
        sources.sort_unstable_by_key(|source| source.id());

        let mut changed_root_count = 0_usize;
        let mut unresolved_change_endpoint = false;
        for source in changes.changed_sources() {
            match root_source(*source, &sources)? {
                Some(_) => {
                    changed_root_count = changed_root_count
                        .checked_add(1)
                        .ok_or(PipelineError::ArithmeticOverflow("changed workspace roots"))?;
                }
                None => unresolved_change_endpoint = true,
            }
        }
        for remap in changes.identity_remaps() {
            match workspace_source_by_locator(&sources, remap.from().source_locator())
                .map(|source| source.id())
                .map(|source| root_source(source, &sources))
                .transpose()?
                .flatten()
            {
                Some(_) => {
                    changed_root_count = changed_root_count
                        .checked_add(1)
                        .ok_or(PipelineError::ArithmeticOverflow("changed workspace roots"))?;
                }
                None => unresolved_change_endpoint = true,
            }
            match workspace_source_by_locator(&sources, remap.to().source_locator())
                .map(|source| source.id())
                .map(|source| root_source(source, &sources))
                .transpose()?
                .flatten()
            {
                Some(_) => {
                    changed_root_count = changed_root_count
                        .checked_add(1)
                        .ok_or(PipelineError::ArithmeticOverflow("changed workspace roots"))?;
                }
                None => unresolved_change_endpoint = true,
            }
        }
        let mut changed_roots =
            reserve_retained_vec(changed_root_count, "changed workspace roots", budget)?;
        for source in changes.changed_sources() {
            if let Some(root) = root_source(*source, &sources)? {
                changed_roots.push(root);
            }
        }
        for remap in changes.identity_remaps() {
            if let Some(root) = workspace_source_by_locator(&sources, remap.from().source_locator())
                .map(|source| source.id())
                .map(|source| root_source(source, &sources))
                .transpose()?
                .flatten()
            {
                changed_roots.push(root);
            }
            if let Some(root) = workspace_source_by_locator(&sources, remap.to().source_locator())
                .map(|source| source.id())
                .map(|source| root_source(source, &sources))
                .transpose()?
                .flatten()
            {
                changed_roots.push(root);
            }
        }
        changed_roots.sort_unstable();
        changed_roots.dedup();
        let graph = unity_asset::reference::ReferenceGraph::build(
            view,
            ReferenceGraphBuildOptions::unbounded(),
            budget,
        )?;
        let context = WorkspaceAnalysisContext::build(view, &graph, budget)?;
        let mut impact = DependencyImpact::default();
        for analysis in cached.values() {
            if analysis.source.workspace_source.is_some_and(|root| {
                changed_roots.binary_search(&root).is_ok()
                    || changes.changed_sources().binary_search(&root).is_ok()
            }) {
                impact.add_analysis_identity(analysis, budget)?;
            }
        }
        for remap in changes.identity_remaps() {
            impact.add_object(remap.from(), budget)?;
            impact.add_object(remap.to(), budget)?;
        }
        for root in &changed_roots {
            let source = workspace_source(&sources, *root)
                .ok_or(PipelineError::UnknownWorkspaceSource(*root))?;
            impact.add_source(source.locator(), budget)?;
        }
        add_current_source_impacts(&mut impact, &sources, &changed_roots, budget)?;
        add_current_object_impacts(&mut impact, &graph, &sources, &changed_roots, budget)?;
        for object in changes.changed_objects() {
            match workspace_source(&sources, object.source()) {
                Some(source) => {
                    if object.binary_path_id().is_none()
                        && object.yaml_file_id().is_none()
                        && object.yaml_document_ordinal().is_none()
                    {
                        return Err(PipelineError::Invariant(
                            "changed object has no supported stable address key",
                        ));
                    }
                    impact.add_changed_object(object, source.locator(), budget)?;
                }
                None => unresolved_change_endpoint = true,
            }
        }
        let changed_child_source = changes.changed_sources().iter().any(|source| {
            workspace_source(&sources, *source)
                .is_some_and(|descriptor| descriptor.parent().is_some())
        });
        let force_full_analysis =
            !graph.is_complete() || changed_child_source || unresolved_change_endpoint;
        let dependency_candidate_assets = cached.len();
        let affected_root_count = cached
            .values()
            .filter(|analysis| {
                let Some(root) = analysis.source.workspace_source else {
                    return false;
                };
                changed_roots.binary_search(&root).is_err()
                    && workspace_source(&sources, root)
                        .is_some_and(|source| source.parent().is_none())
                    && (force_full_analysis
                        || !analysis.complete
                        || impact.matches_analysis(analysis))
            })
            .count();
        let mut affected_roots =
            reserve_retained_vec(affected_root_count, "affected workspace roots", budget)?;
        for analysis in cached.values() {
            let Some(root) = analysis.source.workspace_source else {
                continue;
            };
            if changed_roots.binary_search(&root).is_err()
                && workspace_source(&sources, root).is_some_and(|source| source.parent().is_none())
                && (force_full_analysis || !analysis.complete || impact.matches_analysis(analysis))
            {
                affected_roots.push(root);
            }
        }
        affected_roots.sort_unstable();
        affected_roots.dedup();
        drop(impact);
        cached.retain(|_, analysis| {
            analysis.source.workspace_source.is_none_or(|source| {
                workspace_source(&sources, source)
                    .is_some_and(|descriptor| descriptor.parent().is_none())
            })
        });

        let analyzer = AssetAnalyzer::new(AnalyzerLimits::default());
        let mut metrics = PipelineBuildMetrics {
            full_dependency_scan: dependency_candidate_assets != 0,
            dependency_candidate_assets: saturating_usize_to_u64(dependency_candidate_assets),
            dependency_closure_assets: saturating_usize_to_u64(affected_roots.len()),
            forced_full_analysis: force_full_analysis,
            ..PipelineBuildMetrics::default()
        };
        let roots = workspace_root_paths(&self.paths, &sources, budget)?;
        let mut workspace_scan_hints =
            reserve_retained_vec(roots.len(), "workspace source scan hint updates", budget)?;
        let non_workspace_assets = cached
            .values()
            .filter(|analysis| analysis.source.workspace_source.is_none())
            .count();
        let final_asset_count = roots
            .len()
            .checked_add(non_workspace_assets)
            .ok_or(PipelineError::ArithmeticOverflow("workspace asset count"))?;
        let mut assets =
            reserve_retained_vec(final_asset_count, "workspace analysis batch", budget)?;

        for (coordinate, relative_path, root_id) in roots {
            let root = workspace_source(&sources, root_id)
                .ok_or(PipelineError::UnknownWorkspaceSource(root_id))?;
            let length = view.source_length(root.id())?;
            let cached_analysis = cached
                .take(coordinate)
                .or_else(|| cached.take_workspace_root(root.id()));
            let source = workspace_read_source(
                &self.paths,
                root,
                Cow::Owned(relative_path),
                length,
                cached_analysis
                    .as_ref()
                    .and_then(|analysis| analysis.source.guid.as_deref()),
                budget,
            )?;
            charge_retained_string(&source.rel_path, "source scan hint path", budget)?;
            workspace_scan_hints.push(source_scan_hint_precharged(&source)?);
            let input = context.asset(root.id())?;
            let analyzed = match cached_analysis {
                Some(cached_analysis)
                    if changed_roots.binary_search(&root.id()).is_err()
                        && cached_analysis.source.workspace_source == Some(root.id())
                        && cached_analysis.source.workspace_fingerprint
                            == Some(root.fingerprint())
                        && cached_analysis.source.relative_path == source.rel_path =>
                {
                    if affected_roots.binary_search(&root.id()).is_err() {
                        metrics.reused_assets = metrics.reused_assets.saturating_add(1);
                        assets.push(cached_analysis);
                        continue;
                    }
                    if cached_analysis.graph_inputs.complete {
                        metrics.graph_refreshed_assets =
                            metrics.graph_refreshed_assets.saturating_add(1);
                        analyzer.refresh_graph_facts(&cached_analysis, input, budget)?
                    } else {
                        analyzer.analyze(&source, Some(input), budget)?
                    }
                }
                _ => analyzer.analyze(&source, Some(input), budget)?,
            };
            metrics.analysis.merge(&analyzed.metrics);
            assets.push(analyzed.analysis);
        }

        for (_, cached_analysis) in cached.into_remaining() {
            if cached_analysis.source.workspace_source.is_some() {
                return Err(PipelineError::Invariant(
                    "cached workspace analysis survived current-root filtering",
                ));
            }
            metrics.reused_assets = metrics.reused_assets.saturating_add(1);
            assets.push(cached_analysis);
        }
        let mut scan_hints =
            self.merge_scan_hints_from_replacements(workspace_scan_hints, budget)?;
        let transactions = canonical_transaction_ids(&transaction_receipts, budget)?;
        let batch = AssetAnalysisBatch::new(
            view.workspace_id(),
            view.revision(),
            transactions,
            assets,
            metrics.analysis,
        );
        retain_scan_hints_for_assets(&mut scan_hints, &batch.assets);
        let workspace =
            AssetWorkspace::with_workspace_id(view.workspace_id(), WorkspaceOptions::lenient())?;
        Ok(PreparedBatch {
            batch,
            scan_hints,
            metrics,
            transaction_receipts,
            workspace: Some(PreparedWorkspace {
                workspace,
                roots: WorkspaceRoots::default(),
                hydrated: false,
            }),
            filesystem_validation: None,
        })
    }

    fn publish_batch(
        &mut self,
        prepared: PreparedBatch,
        transaction: Option<TransactionId>,
        target_revision: Option<WorkspaceRevision>,
        started: Instant,
        budget: &mut AssetLoadBudget,
    ) -> Result<PipelineBuildOutput, PipelineError> {
        let PreparedBatch {
            batch,
            scan_hints,
            mut metrics,
            transaction_receipts,
            workspace,
            filesystem_validation,
        } = prepared;
        if self.active_compatibility_match
            && self
                .source_state
                .as_ref()
                .is_some_and(|state| batch_matches_state(&batch, state))
            && self.store.active().is_some_and(|active| {
                active
                    .transaction_receipts()
                    .matches_canonical_ids(&batch.transactions)
            })
        {
            if let Some(validation) = filesystem_validation.as_ref() {
                #[cfg(test)]
                Self::run_scan_validation_hook(
                    &mut self.scan_validation_hook,
                    ScanValidationCheckpoint::NoChangePreReturn,
                );
                self.scanner
                    .validate_scan(validation)
                    .map_err(|error| PipelineError::Scan(Box::new(error)))?;
            }
            self.active_compatibility_match = true;
            self.install_workspace(workspace);
            let warnings = self.take_pending_publish_warnings();
            return Ok(PipelineBuildOutput {
                disposition: PipelineBuildDisposition::NoChange,
                active: self.active.clone(),
                metrics,
                disk_estimate: None,
                warnings,
                transaction,
                target_revision,
                duration_ms: started.elapsed().as_millis(),
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
        metrics.projection = projection.metrics;
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
            Err(primary) => return Err(self.abort_generation_build(&mut build, primary)),
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
                return Err(self.abort_generation_build(&mut build, primary));
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
        if let Some(validation) = filesystem_validation.as_ref() {
            #[cfg(test)]
            Self::run_scan_validation_hook(
                &mut self.scan_validation_hook,
                ScanValidationCheckpoint::ActivationPreCommit,
            );
            self.scanner
                .validate_scan(validation)
                .map_err(|error| PipelineError::Scan(Box::new(error)))?;
        }
        let report = match prepared.activate_with_budget(budget) {
            Ok(report) => report,
            Err(error) => {
                self.record_store_reopen_requirement(&error);
                return Err(PipelineError::Store(Box::new(error)));
            }
        };
        self.append_pending_publish_warnings(report.warnings);
        let warnings = self.take_pending_publish_warnings();
        self.source_state = Some(source_state);
        self.active = Some(Arc::clone(&active));
        self.active_analysis_match = true;
        self.active_compatibility_match = true;
        self.rebuild_bootstrap = None;
        self.install_workspace(workspace);

        Ok(PipelineBuildOutput {
            disposition: PipelineBuildDisposition::Published,
            active: Some(active),
            metrics,
            disk_estimate: Some(disk_estimate),
            warnings,
            transaction,
            target_revision,
            duration_ms: started.elapsed().as_millis(),
        })
    }

    fn clone_previous_assets(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<CachedAssets, PipelineError> {
        let Some(state) = &self.source_state else {
            return Ok(CachedAssets {
                entries: Vec::new(),
                workspace_roots: Vec::new(),
            });
        };
        for analysis in state.assets() {
            charge_cached_analysis_clone(analysis, budget)?;
        }
        let mut entries = reserve_retained_vec(state.assets().len(), "cached asset index", budget)?;
        for analysis in state.assets() {
            entries.push(CachedAsset {
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
        Ok(CachedAssets {
            entries,
            workspace_roots,
        })
    }

    fn merge_scan_hints(
        &self,
        deleted: &[ProjectSourcePath],
        read_sources: &[ScannedSource],
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceScanHint>, PipelineError> {
        let previous = self
            .source_state
            .as_ref()
            .map_or(&[][..], |state| state.scan_hints());
        let update_count = deleted.len().checked_add(read_sources.len()).ok_or(
            PipelineError::ArithmeticOverflow("source scan hint update count"),
        )?;
        let mut updates = reserve_retained_vec(update_count, "source scan hint updates", budget)?;
        updates.extend(
            deleted
                .iter()
                .enumerate()
                .map(|(deleted_index, _)| ScanHintUpdate::Delete { deleted_index }),
        );
        updates.extend(
            read_sources
                .iter()
                .enumerate()
                .map(|(source_index, _)| ScanHintUpdate::Upsert { source_index }),
        );
        updates.sort_unstable_by(|left, right| {
            left.coordinate(deleted, read_sources)
                .cmp(&right.coordinate(deleted, read_sources))
        });
        if updates.windows(2).any(|pair| {
            pair[0].coordinate(deleted, read_sources) == pair[1].coordinate(deleted, read_sources)
        }) {
            return Err(PipelineError::Invariant(
                "filesystem scan produced duplicate scan hint updates",
            ));
        }

        let mut previous_index = 0;
        let mut final_count = 0_usize;
        for update in &updates {
            let coordinate = update.coordinate(deleted, read_sources);
            while previous_index < previous.len()
                && previous[previous_index].coordinate < coordinate
            {
                final_count = final_count
                    .checked_add(1)
                    .ok_or(PipelineError::ArithmeticOverflow("source scan hint count"))?;
                previous_index += 1;
            }
            if previous_index < previous.len() && previous[previous_index].coordinate == coordinate
            {
                previous_index += 1;
            }
            if matches!(update, ScanHintUpdate::Upsert { .. }) {
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
            let coordinate = update.coordinate(deleted, read_sources);
            while previous_index < previous.len()
                && previous[previous_index].coordinate < coordinate
            {
                push_cloned_scan_hint(&mut merged, &previous[previous_index], budget)?;
                previous_index += 1;
            }
            if previous_index < previous.len() && previous[previous_index].coordinate == coordinate
            {
                previous_index += 1;
            }
            if let ScanHintUpdate::Upsert { source_index } = update {
                let source = &read_sources[source_index].source;
                charge_retained_string(&source.rel_path, "source scan hint path", budget)?;
                merged.push(source_scan_hint_precharged(source)?);
            }
        }
        while previous_index < previous.len() {
            push_cloned_scan_hint(&mut merged, &previous[previous_index], budget)?;
            previous_index += 1;
        }
        debug_assert_eq!(merged.len(), final_count);
        Ok(merged)
    }

    fn merge_scan_hints_from_replacements(
        &self,
        mut replacements: Vec<SourceScanHint>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceScanHint>, PipelineError> {
        let previous = self
            .source_state
            .as_ref()
            .map_or(&[][..], |state| state.scan_hints());
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

    fn mark_desired_revision(
        &mut self,
        desired: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PipelineError> {
        let warnings = match self.store.record_desired_revision(desired, budget) {
            Ok(warnings) => warnings,
            Err(error) => {
                self.record_store_reopen_requirement(&error);
                return Err(PipelineError::Store(Box::new(error)));
            }
        };
        self.append_pending_publish_warnings(warnings);
        if let Some(active) = &mut self.active {
            Arc::make_mut(active).set_desired_revision(desired);
        }
        Ok(())
    }

    fn take_pending_publish_warnings(&mut self) -> Vec<String> {
        const OMITTED_WARNING: &str =
            "additional publish warnings were omitted to satisfy the protocol budget";

        let mut warnings = std::mem::take(&mut self.pending_publish_warnings);
        let mut omitted = std::mem::take(&mut self.pending_publish_warnings_omitted);
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

    fn append_pending_publish_warnings(
        &mut self,
        warnings: impl IntoIterator<Item = GenerationPublishWarning>,
    ) {
        for warning in warnings {
            let kind = warning.kind();
            let message = warning.to_string();
            if matches!(
                kind,
                GenerationPublishWarningKind::PreparationCleanup
                    | GenerationPublishWarningKind::PostCommitDurability
                    | GenerationPublishWarningKind::PostCommitCleanup
            ) {
                self.record_generation_cleanup_failure(message.clone());
            }
            if self.pending_publish_warnings.len() < MAX_REINDEX_PUBLISH_WARNINGS {
                self.pending_publish_warnings
                    .push(crate::wire::bounded_publish_warning(message));
            } else {
                self.pending_publish_warnings_omitted = true;
            }
        }
    }

    fn abort_generation_build(
        &mut self,
        build: &mut GenerationBuild,
        primary: PipelineError,
    ) -> PipelineError {
        let mut cleanup_budget = AssetLoadBudget::default();
        match build.abort_with_budget(&mut cleanup_budget) {
            Ok(()) => primary,
            Err(cleanup) => {
                self.record_generation_cleanup_failure(cleanup.to_string());
                PipelineError::StagingAbortFailed {
                    primary: Box::new(primary),
                    cleanup: Box::new(cleanup),
                }
            }
        }
    }

    fn record_generation_cleanup_failure(&mut self, message: String) {
        self.generation_maintenance = GenerationMaintenanceStatus {
            state: GenerationMaintenanceState::RecoveryRequired,
            last_recovered_entries: self.generation_maintenance.last_recovered_entries,
            last_cleanup_failure: Some(crate::wire::bounded_error_message(message)),
        };
    }

    fn record_store_reopen_requirement(&mut self, error: &GenerationStoreError) {
        if error.requires_reopen() {
            self.record_generation_cleanup_failure(error.to_string());
        }
    }

    fn record_generation_recovery(&mut self, removed_entries: u64) {
        self.generation_maintenance = GenerationMaintenanceStatus {
            state: GenerationMaintenanceState::Clean,
            last_recovered_entries: removed_entries,
            last_cleanup_failure: None,
        };
    }

    fn install_workspace(&mut self, prepared: Option<PreparedWorkspace>) {
        let Some(prepared) = prepared else {
            return;
        };
        self.workspace = prepared.workspace;
        self.workspace_roots = prepared.roots;
        self.workspace_hydrated = prepared.hydrated;
    }
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

struct PreparedBatch {
    batch: AssetAnalysisBatch,
    scan_hints: Vec<SourceScanHint>,
    metrics: PipelineBuildMetrics,
    transaction_receipts: TransactionReceiptWindow,
    workspace: Option<PreparedWorkspace>,
    filesystem_validation: Option<ScanValidation>,
}

struct ScannedSource {
    source: ReadSource,
    diagnostics: Vec<ScanDiagnostic>,
}

struct PreparedWorkspace {
    workspace: AssetWorkspace,
    roots: WorkspaceRoots,
    hydrated: bool,
}

enum FilesystemAdmissionEffect {
    Unload {
        source_id: SourceId,
    },
    Load {
        source_index: usize,
        root_update_index: usize,
    },
}

struct FilesystemReloadPlan {
    source_index: usize,
    existing_root: Option<SourceId>,
    load: bool,
}

enum FilesystemRootUpdate {
    Delete {
        deleted_index: usize,
    },
    Source {
        source_index: usize,
        replacement: Option<SourceId>,
    },
}

impl FilesystemRootUpdate {
    fn coordinate(
        &self,
        deleted: &[ProjectSourcePath],
        read_sources: &[ScannedSource],
    ) -> IndexedSourceCoordinate {
        match self {
            Self::Delete { deleted_index } => deleted[*deleted_index].coordinate(),
            Self::Source { source_index, .. } => read_sources[*source_index].source.coordinate,
        }
    }

    const fn replacement(&self) -> Option<SourceId> {
        match self {
            Self::Delete { .. } => None,
            Self::Source { replacement, .. } => *replacement,
        }
    }

    fn set_replacement(&mut self, source_id: SourceId) -> Result<(), PipelineError> {
        match self {
            Self::Source { replacement, .. } => {
                *replacement = Some(source_id);
                Ok(())
            }
            Self::Delete { .. } => Err(PipelineError::Invariant(
                "source admission attempted to replace a deleted workspace root",
            )),
        }
    }
}

enum ScanHintUpdate {
    Delete { deleted_index: usize },
    Upsert { source_index: usize },
}

impl ScanHintUpdate {
    fn coordinate(
        &self,
        deleted: &[ProjectSourcePath],
        read_sources: &[ScannedSource],
    ) -> IndexedSourceCoordinate {
        match self {
            Self::Delete { deleted_index } => deleted[*deleted_index].coordinate(),
            Self::Upsert { source_index } => read_sources[*source_index].source.coordinate,
        }
    }
}

#[derive(Default)]
struct WorkspaceRoots {
    entries: Vec<(IndexedSourceCoordinate, SourceId)>,
}

impl WorkspaceRoots {
    fn get(&self, coordinate: IndexedSourceCoordinate) -> Option<&SourceId> {
        let index = self
            .entries
            .binary_search_by_key(&coordinate, |(candidate, _)| *candidate)
            .ok()?;
        Some(&self.entries[index].1)
    }

    fn contains_key(&self, coordinate: IndexedSourceCoordinate) -> bool {
        self.get(coordinate).is_some()
    }
}

fn merge_workspace_roots(
    previous: &WorkspaceRoots,
    updates: &[FilesystemRootUpdate],
    deleted: &[ProjectSourcePath],
    read_sources: &[ScannedSource],
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceRoots, PipelineError> {
    let mut previous_index = 0;
    let mut final_count = 0_usize;
    for update in updates {
        let coordinate = update.coordinate(deleted, read_sources);
        while previous_index < previous.entries.len()
            && previous.entries[previous_index].0 < coordinate
        {
            final_count = final_count
                .checked_add(1)
                .ok_or(PipelineError::ArithmeticOverflow("workspace root count"))?;
            previous_index += 1;
        }
        if previous_index < previous.entries.len()
            && previous.entries[previous_index].0 == coordinate
        {
            previous_index += 1;
        }
        if update.replacement().is_some() {
            final_count = final_count
                .checked_add(1)
                .ok_or(PipelineError::ArithmeticOverflow("workspace root count"))?;
        }
    }
    final_count = final_count
        .checked_add(previous.entries.len().saturating_sub(previous_index))
        .ok_or(PipelineError::ArithmeticOverflow("workspace root count"))?;

    let mut entries = reserve_retained_vec(final_count, "workspace root index", budget)?;
    previous_index = 0;
    for update in updates {
        let coordinate = update.coordinate(deleted, read_sources);
        while previous_index < previous.entries.len()
            && previous.entries[previous_index].0 < coordinate
        {
            entries.push(previous.entries[previous_index]);
            previous_index += 1;
        }
        if previous_index < previous.entries.len()
            && previous.entries[previous_index].0 == coordinate
        {
            previous_index += 1;
        }
        if let Some(root) = update.replacement() {
            entries.push((coordinate, root));
        }
    }
    while previous_index < previous.entries.len() {
        entries.push(previous.entries[previous_index]);
        previous_index += 1;
    }
    debug_assert_eq!(entries.len(), final_count);
    Ok(WorkspaceRoots { entries })
}

struct CachedAsset {
    coordinate: IndexedSourceCoordinate,
    analysis: Option<AssetAnalysis>,
}

struct CachedAssets {
    entries: Vec<CachedAsset>,
    workspace_roots: Vec<(SourceId, usize)>,
}

impl CachedAssets {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            workspace_roots: Vec::new(),
        }
    }

    fn get(&self, coordinate: IndexedSourceCoordinate) -> Option<&AssetAnalysis> {
        let index = self
            .entries
            .binary_search_by_key(&coordinate, |entry| entry.coordinate)
            .ok()?;
        self.entries[index].analysis.as_ref()
    }

    fn contains_key(&self, coordinate: IndexedSourceCoordinate) -> bool {
        self.get(coordinate).is_some()
    }

    fn take(&mut self, coordinate: IndexedSourceCoordinate) -> Option<AssetAnalysis> {
        let index = self
            .entries
            .binary_search_by_key(&coordinate, |entry| entry.coordinate)
            .ok()?;
        self.entries[index].analysis.take()
    }

    fn remove(&mut self, coordinate: IndexedSourceCoordinate) {
        let _ = self.take(coordinate);
    }

    fn take_workspace_root(&mut self, root: SourceId) -> Option<AssetAnalysis> {
        let index = self
            .workspace_roots
            .binary_search_by_key(&root, |(candidate, _)| *candidate)
            .ok()
            .map(|index| self.workspace_roots[index].1)?;
        self.entries[index].analysis.take()
    }

    fn values(&self) -> impl Iterator<Item = &AssetAnalysis> {
        self.entries
            .iter()
            .filter_map(|entry| entry.analysis.as_ref())
    }

    fn iter(&self) -> impl Iterator<Item = (IndexedSourceCoordinate, &AssetAnalysis)> {
        self.entries.iter().filter_map(|entry| {
            entry
                .analysis
                .as_ref()
                .map(|analysis| (entry.coordinate, analysis))
        })
    }

    fn retain(
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

    fn len(&self) -> usize {
        self.values().count()
    }

    fn into_remaining(self) -> impl Iterator<Item = (IndexedSourceCoordinate, AssetAnalysis)> {
        self.entries
            .into_iter()
            .filter_map(|entry| entry.analysis.map(|analysis| (entry.coordinate, analysis)))
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

fn batch_matches_state(batch: &AssetAnalysisBatch, state: &SourceStateSnapshot) -> bool {
    batch.workspace == state.workspace()
        && batch.revision == state.revision()
        && batch.assets.as_slice() == state.assets()
}

fn canonical_transaction_ids(
    receipts: &TransactionReceiptWindow,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<TransactionId>, PipelineError> {
    let mut transactions =
        reserve_retained_vec(receipts.ids().len(), "canonical transaction IDs", budget)?;
    transactions.extend(receipts.ids());
    transactions.sort_unstable();
    Ok(transactions)
}

fn validate_change_set_view(
    changes: &ChangeSet,
    view: &dyn WorkspaceView,
) -> Result<(), PipelineError> {
    if changes.workspace() != view.workspace_id() {
        return Err(PipelineError::WorkspaceMismatch {
            expected: changes.workspace(),
            actual: view.workspace_id(),
        });
    }
    if changes.to_revision() != view.revision() {
        return Err(PipelineError::ViewRevisionMismatch {
            expected: changes.to_revision(),
            actual: view.revision(),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ChangedObjectImpact<'source> {
    object: &'source ObjectId,
    locator: &'source SourceLocator,
}

#[derive(Default)]
struct DependencyImpact<'source> {
    guids: Vec<&'source str>,
    objects: Vec<&'source ObjectAddress>,
    changed_objects: Vec<ChangedObjectImpact<'source>>,
    sources: Vec<&'source SourceLocator>,
    source_paths: Vec<&'source str>,
}

impl<'source> DependencyImpact<'source> {
    fn add_guid(
        &mut self,
        guid: Option<&'source str>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PipelineError> {
        if let Some(guid) = guid.filter(|guid| !guid.is_empty()) {
            push_unique_retained(&mut self.guids, guid, "dependency GUIDs", budget)?;
        }
        Ok(())
    }

    fn add_analysis_identity(
        &mut self,
        analysis: &'source AssetAnalysis,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PipelineError> {
        self.add_guid(analysis.source.guid.as_deref(), budget)?;
        if let Some(locator) = &analysis.source.locator {
            self.add_source(locator, budget)?;
        }
        for object in &analysis.graph_inputs.objects {
            self.add_object(&object.address, budget)?;
        }
        Ok(())
    }

    fn add_object(
        &mut self,
        object: &'source ObjectAddress,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PipelineError> {
        push_unique_retained(&mut self.objects, object, "dependency objects", budget)
    }

    fn add_changed_object(
        &mut self,
        object: &'source ObjectId,
        locator: &'source SourceLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PipelineError> {
        push_unique_retained(
            &mut self.changed_objects,
            ChangedObjectImpact { object, locator },
            "changed dependency objects",
            budget,
        )
    }

    fn add_source(
        &mut self,
        source: &'source SourceLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PipelineError> {
        push_unique_retained(&mut self.sources, source, "dependency sources", budget)
    }

    fn add_source_path(
        &mut self,
        path: &'source str,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PipelineError> {
        push_unique_retained(
            &mut self.source_paths,
            path,
            "dependency source paths",
            budget,
        )
    }

    fn matches_analysis(&self, analysis: &AssetAnalysis) -> bool {
        analysis.references.iter().any(|reference| {
            reference.dependency_keys.iter().any(|key| match key {
                ReferenceDependencyKey::Guid { guid, .. } => self
                    .guids
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(guid)),
                ReferenceDependencyKey::Object { address } => self.matches_object(address),
                ReferenceDependencyKey::Source { locator } => self.matches_source(locator),
            }) || match &reference.resolution {
                ReferenceResolutionProjection::Resolved { target } => self.matches_object(target),
                ReferenceResolutionProjection::Unloaded { source } => source
                    .as_ref()
                    .is_some_and(|source| self.matches_source(source)),
                ReferenceResolutionProjection::Missing { target } => target
                    .as_ref()
                    .is_some_and(|target| self.matches_object(target)),
                ReferenceResolutionProjection::Ambiguous { candidates } => candidates
                    .iter()
                    .any(|candidate| self.matches_object(candidate)),
                ReferenceResolutionProjection::Null | ReferenceResolutionProjection::Invalid => {
                    false
                }
            }
        })
    }

    fn matches_source(&self, source: &SourceLocator) -> bool {
        self.sources.contains(&source)
            || self
                .source_paths
                .iter()
                .any(|path| source.members().is_empty() && source.root_alias().as_str() == *path)
    }

    fn matches_object(&self, address: &ObjectAddress) -> bool {
        self.objects.contains(&address)
            || self.changed_objects.iter().any(|candidate| {
                address.source_locator() == candidate.locator
                    && address.binary_path_id() == candidate.object.binary_path_id()
                    && address.yaml_file_id() == candidate.object.yaml_file_id()
                    && address.yaml_document_ordinal() == candidate.object.yaml_document_ordinal()
            })
    }
}

fn push_unique_retained<T: PartialEq>(
    values: &mut Vec<T>,
    value: T,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    if values.contains(&value) {
        return Ok(());
    }
    prepare_retained_vec_push(values, resource, budget)?;
    values.push(value);
    Ok(())
}

fn add_current_object_impacts<'source>(
    impact: &mut DependencyImpact<'source>,
    graph: &'source unity_asset::reference::ReferenceGraph,
    sources: &'source [WorkspaceSource],
    changed_roots: &[SourceId],
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    for node in graph.nodes() {
        let Some(root) = root_source(node.object().source(), sources)? else {
            continue;
        };
        if changed_roots.binary_search(&root).is_ok() {
            impact.add_object(graph.address(node)?, budget)?;
        }
    }
    Ok(())
}

fn add_current_source_impacts<'source>(
    impact: &mut DependencyImpact<'source>,
    sources: &'source [WorkspaceSource],
    changed_roots: &[SourceId],
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    for source in sources {
        let Some(root) = root_source(source.id(), sources)? else {
            continue;
        };
        if changed_roots.binary_search(&root).is_ok() {
            impact.add_source(source.locator(), budget)?;
        }
    }
    Ok(())
}

fn workspace_source(sources: &[WorkspaceSource], source: SourceId) -> Option<&WorkspaceSource> {
    sources
        .binary_search_by_key(&source, |candidate| candidate.id())
        .ok()
        .map(|index| &sources[index])
}

fn workspace_source_by_locator<'source>(
    sources: &'source [WorkspaceSource],
    locator: &SourceLocator,
) -> Option<&'source WorkspaceSource> {
    sources.iter().find(|source| source.locator() == locator)
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

fn prepare_retained_vec_push<T>(
    values: &mut Vec<T>,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    let requested = values
        .len()
        .checked_add(1)
        .ok_or(PipelineError::ArithmeticOverflow(resource))?;
    let additional_bytes = if values.len() == values.capacity() {
        vec_allocation_bytes::<T>(1).map_err(|_| PipelineError::ArithmeticOverflow(resource))?
    } else {
        0
    };
    budget.check_entries(1)?;
    budget.check_members(1)?;
    budget.check_bytes(additional_bytes)?;
    budget.consume_entries(1)?;
    budget.consume_members(1)?;
    budget.consume_bytes(additional_bytes)?;
    if values.len() == values.capacity() {
        values
            .try_reserve_exact(1)
            .map_err(|source| PipelineError::Allocation {
                resource,
                requested,
                unit: "elements",
                source,
            })?;
    }
    Ok(())
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
    let mut cloned = reserve_precharged_string(value.len(), resource)?;
    cloned.push_str(value);
    Ok(cloned)
}

fn clone_checked_string(
    value: &str,
    resource: &'static str,
    budget: &AssetLoadBudget,
) -> Result<String, PipelineError> {
    let planned = string_allocation_bytes(value.len())
        .map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    budget.check_bytes(planned)?;
    let cloned = clone_precharged_string(value, resource)?;
    let retained = string_allocation_bytes(cloned.capacity())
        .map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    budget.check_bytes(retained)?;
    Ok(cloned)
}

fn reserve_precharged_string(
    capacity: usize,
    resource: &'static str,
) -> Result<String, PipelineError> {
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(capacity)
        .map_err(|source| PipelineError::Allocation {
            resource,
            requested: capacity,
            unit: "bytes",
            source,
        })?;
    Ok(cloned)
}

fn retained_display_string(
    value: &impl fmt::Display,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, PipelineError> {
    let mut counter = CountingFormatter::default();
    fmt::write(&mut counter, format_args!("{value}"))
        .map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    let bytes = string_allocation_bytes(counter.bytes)
        .map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    let mut rendered = reserve_precharged_string(counter.bytes, resource)?;
    fmt::write(&mut rendered, format_args!("{value}"))
        .map_err(|_| PipelineError::Invariant("formatting into a String failed"))?;
    Ok(rendered)
}

fn clone_precharged_path(value: &Path, resource: &'static str) -> Result<PathBuf, PipelineError> {
    let requested = value.as_os_str().len();
    let mut cloned = PathBuf::new();
    cloned
        .try_reserve_exact(requested)
        .map_err(|source| PipelineError::Allocation {
            resource,
            requested,
            unit: "bytes",
            source,
        })?;
    cloned.push(value);
    Ok(cloned)
}

fn clone_checked_path(
    value: &Path,
    resource: &'static str,
    budget: &AssetLoadBudget,
) -> Result<PathBuf, PipelineError> {
    let planned = u64::try_from(value.as_os_str().len())
        .map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    budget.check_bytes(planned)?;
    let cloned = clone_precharged_path(value, resource)?;
    let retained = u64::try_from(cloned.capacity())
        .map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    budget.check_bytes(retained)?;
    Ok(cloned)
}

#[derive(Default)]
struct CountingFormatter {
    bytes: usize,
}

impl fmt::Write for CountingFormatter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.bytes = self.bytes.checked_add(value.len()).ok_or(fmt::Error)?;
        Ok(())
    }
}

struct BoundedFormatter<'output> {
    output: &'output mut String,
    maximum_bytes: usize,
    truncated: bool,
}

impl fmt::Write for BoundedFormatter<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.truncated {
            return Ok(());
        }
        let remaining = self.maximum_bytes.saturating_sub(self.output.len());
        if remaining == 0 {
            self.truncated = !value.is_empty();
            return Ok(());
        }
        let mut end = remaining.min(value.len());
        while end != 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        self.output.push_str(&value[..end]);
        self.truncated = end != value.len();
        Ok(())
    }
}

fn bounded_format_len(
    arguments: fmt::Arguments<'_>,
    maximum_bytes: usize,
    resource: &'static str,
) -> Result<usize, PipelineError> {
    let mut counter = CountingFormatter::default();
    fmt::write(&mut counter, arguments).map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    Ok(counter.bytes.min(maximum_bytes))
}

fn bounded_format_precharged(
    arguments: fmt::Arguments<'_>,
    capacity: usize,
    resource: &'static str,
) -> Result<String, PipelineError> {
    let mut output = reserve_precharged_string(capacity, resource)?;
    fmt::write(
        &mut BoundedFormatter {
            output: &mut output,
            maximum_bytes: capacity,
            truncated: false,
        },
        arguments,
    )
    .map_err(|_| PipelineError::Invariant("formatting into a String failed"))?;
    Ok(output)
}

fn prepare_analysis_annotation(
    analysis: &mut AssetAnalysis,
    code: &str,
    message_bytes: usize,
    with_truncation: bool,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    let members = if with_truncation { 2 } else { 1 };
    let mut bytes = string_allocation_bytes(code.len())
        .map_err(|_| PipelineError::ArithmeticOverflow("analysis diagnostic"))?
        .checked_add(
            string_allocation_bytes(message_bytes)
                .map_err(|_| PipelineError::ArithmeticOverflow("analysis diagnostic"))?,
        )
        .ok_or(PipelineError::ArithmeticOverflow("analysis diagnostic"))?;
    if analysis.diagnostics.len() == analysis.diagnostics.capacity() {
        bytes = bytes
            .checked_add(
                vec_allocation_bytes::<Diagnostic>(1)
                    .map_err(|_| PipelineError::ArithmeticOverflow("analysis diagnostic"))?,
            )
            .ok_or(PipelineError::ArithmeticOverflow("analysis diagnostic"))?;
    }
    if with_truncation && analysis.truncations.len() == analysis.truncations.capacity() {
        bytes = bytes
            .checked_add(
                vec_allocation_bytes::<AnalysisTruncation>(1)
                    .map_err(|_| PipelineError::ArithmeticOverflow("analysis truncation"))?,
            )
            .ok_or(PipelineError::ArithmeticOverflow("analysis truncation"))?;
    }
    budget.check_entries(members)?;
    budget.check_members(members)?;
    budget.check_bytes(bytes)?;
    budget.consume_entries(members)?;
    budget.consume_members(members)?;
    budget.consume_bytes(bytes)?;
    reserve_precharged_vec_push(&mut analysis.diagnostics, "analysis diagnostic")?;
    if with_truncation {
        reserve_precharged_vec_push(&mut analysis.truncations, "analysis truncation")?;
    }
    Ok(())
}

fn reserve_precharged_vec_push<T>(
    values: &mut Vec<T>,
    resource: &'static str,
) -> Result<(), PipelineError> {
    if values.len() != values.capacity() {
        return Ok(());
    }
    let requested = values
        .len()
        .checked_add(1)
        .ok_or(PipelineError::ArithmeticOverflow(resource))?;
    values
        .try_reserve_exact(1)
        .map_err(|source| PipelineError::Allocation {
            resource,
            requested,
            unit: "elements",
            source,
        })
}

fn clone_known_paths(
    source_state: Option<&SourceStateSnapshot>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ProjectSourcePath>, PipelineError> {
    let assets = source_state.map_or(&[][..], SourceStateSnapshot::assets);
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

fn workspace_root_paths(
    paths: &IndexPaths,
    sources: &[WorkspaceSource],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<(IndexedSourceCoordinate, String, SourceId)>, PipelineError> {
    let root_count = sources
        .iter()
        .filter(|source| source.parent().is_none())
        .count();
    for root in sources.iter().filter(|source| source.parent().is_none()) {
        let retained_bytes = match workspace_root_project_path(paths, root)? {
            Some(relative) => portable_relative_path_len(relative.as_relative_path())?,
            None => root.locator().root_alias().as_str().len(),
        };
        let retained_bytes = u64::try_from(retained_bytes)
            .map_err(|_| PipelineError::ArithmeticOverflow("workspace root path"))?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_bytes(retained_bytes)?;
    }
    let mut by_path = reserve_retained_vec(root_count, "workspace root paths", budget)?;
    for root in sources.iter().filter(|source| source.parent().is_none()) {
        let (coordinate, relative_path) = match workspace_root_project_path(paths, root)? {
            Some(relative) => (
                IndexedSourceCoordinate::project(relative.identity()),
                portable_relative_path_precharged(relative.as_relative_path())?,
            ),
            None => (
                IndexedSourceCoordinate::workspace(root.id()),
                clone_precharged_string(
                    root.locator().root_alias().as_str(),
                    "workspace root path",
                )?,
            ),
        };
        by_path.push((coordinate, relative_path, root.id()));
    }
    sort_workspace_root_paths(&mut by_path)?;
    Ok(by_path)
}

fn sort_workspace_root_paths(
    by_path: &mut Vec<(IndexedSourceCoordinate, String, SourceId)>,
) -> Result<(), PipelineError> {
    by_path.sort_unstable_by_key(|(coordinate, _, source)| (*coordinate, *source));
    for index in 1..by_path.len() {
        if by_path[index - 1].0 == by_path[index].0 {
            let (_, relative_path, second) = by_path.remove(index);
            let first = by_path[index - 1].2;
            return Err(PipelineError::RelativePathCollision {
                relative_path,
                first,
                second,
            });
        }
    }
    Ok(())
}

fn workspace_root_project_path(
    paths: &IndexPaths,
    root: &WorkspaceSource,
) -> Result<Option<ProjectPath>, PipelineError> {
    let Some(origin) = root.physical_origin() else {
        return Ok(None);
    };
    match paths.project_path_space().resolve(origin) {
        Ok(Some(relative)) => {
            paths
                .project_authority()
                .validate_parent_lookup(relative.as_relative_path())
                .map_err(|source| {
                    PipelineError::Scan(Box::new(ScanError::TraversalRead { source }))
                })?;
            Ok(Some(relative))
        }
        Ok(None) => Ok(None),
        Err(ProjectPathError::OutsideProject { .. }) => Ok(None),
        Err(error) => Err(PipelineError::Configuration(anyhow::Error::new(error))),
    }
}

fn portable_relative_path_len(path: &Path) -> Result<usize, PipelineError> {
    let mut bytes = 0_usize;
    let mut count = 0_usize;
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(PipelineError::NonPortableWorkspacePath(path.to_path_buf()));
        };
        let component = component
            .to_str()
            .ok_or_else(|| PipelineError::NonPortableWorkspacePath(path.to_path_buf()))?;
        if component.is_empty() {
            return Err(PipelineError::NonPortableWorkspacePath(path.to_path_buf()));
        }
        if count != 0 {
            bytes = bytes
                .checked_add(1)
                .ok_or(PipelineError::ArithmeticOverflow("portable workspace path"))?;
        }
        bytes = bytes
            .checked_add(component.len())
            .ok_or(PipelineError::ArithmeticOverflow("portable workspace path"))?;
        count = count
            .checked_add(1)
            .ok_or(PipelineError::ArithmeticOverflow("portable workspace path"))?;
    }
    if count == 0 {
        return Err(PipelineError::NonPortableWorkspacePath(path.to_path_buf()));
    }
    Ok(bytes)
}

fn portable_relative_path_precharged(path: &Path) -> Result<String, PipelineError> {
    let retained_bytes = portable_relative_path_len(path)?;
    let mut relative_path = reserve_precharged_string(retained_bytes, "portable workspace path")?;
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(PipelineError::NonPortableWorkspacePath(path.to_path_buf()));
        };
        let component = component
            .to_str()
            .ok_or_else(|| PipelineError::NonPortableWorkspacePath(path.to_path_buf()))?;
        if !relative_path.is_empty() {
            relative_path.push('/');
        }
        relative_path.push_str(component);
    }
    Ok(relative_path)
}

fn root_source(
    source: SourceId,
    sources: &[WorkspaceSource],
) -> Result<Option<SourceId>, PipelineError> {
    let Some(_) = workspace_source(sources, source) else {
        return Ok(None);
    };
    let mut current = source;
    for _ in 0..=sources.len() {
        let descriptor = workspace_source(sources, current)
            .ok_or(PipelineError::UnknownWorkspaceSource(current))?;
        match descriptor.parent() {
            Some(parent) => current = parent,
            None => return Ok(Some(current)),
        }
    }
    Err(PipelineError::SourceHierarchyCycle(source))
}

fn workspace_read_source(
    paths: &IndexPaths,
    source: &WorkspaceSource,
    relative_path: Cow<'_, str>,
    length: u64,
    guid: Option<&str>,
    budget: &mut AssetLoadBudget,
) -> Result<ReadSource, PipelineError> {
    let coordinate = match workspace_root_project_path(paths, source)? {
        Some(path) => IndexedSourceCoordinate::project(path.identity()),
        None => IndexedSourceCoordinate::workspace(source.id()),
    };
    let name = Path::new(relative_path.as_ref())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(relative_path.as_ref());
    let abs_path_capacity = match source.physical_origin() {
        Some(origin) => origin.as_os_str().len(),
        None => paths
            .project_root()
            .as_os_str()
            .len()
            .checked_add(relative_path.len())
            .and_then(|bytes| bytes.checked_add(1))
            .ok_or(PipelineError::ArithmeticOverflow("workspace source path"))?,
    };
    let abs_path_bytes = u64::try_from(abs_path_capacity)
        .map_err(|_| PipelineError::ArithmeticOverflow("workspace source path"))?;
    budget.check_bytes(abs_path_bytes)?;
    budget.check_bytes(
        u64::try_from(name.len())
            .map_err(|_| PipelineError::ArithmeticOverflow("workspace source name"))?,
    )?;
    if let Some(guid) = guid {
        budget.check_bytes(
            u64::try_from(guid.len())
                .map_err(|_| PipelineError::ArithmeticOverflow("workspace source GUID"))?,
        )?;
    }
    if matches!(&relative_path, Cow::Borrowed(_)) {
        budget
            .check_bytes(u64::try_from(relative_path.len()).map_err(|_| {
                PipelineError::ArithmeticOverflow("workspace source relative path")
            })?)?;
    }
    budget.consume_bytes(abs_path_bytes)?;
    budget.consume_bytes(
        u64::try_from(name.len())
            .map_err(|_| PipelineError::ArithmeticOverflow("workspace source name"))?,
    )?;
    if let Some(guid) = guid {
        budget.consume_bytes(
            u64::try_from(guid.len())
                .map_err(|_| PipelineError::ArithmeticOverflow("workspace source GUID"))?,
        )?;
    }
    if matches!(&relative_path, Cow::Borrowed(_)) {
        budget
            .consume_bytes(u64::try_from(relative_path.len()).map_err(|_| {
                PipelineError::ArithmeticOverflow("workspace source relative path")
            })?)?;
    }

    let abs_path = match source.physical_origin() {
        Some(origin) => clone_precharged_path(origin, "workspace source path")?,
        None => {
            let mut path = PathBuf::new();
            path.try_reserve_exact(abs_path_capacity)
                .map_err(|allocation| PipelineError::Allocation {
                    resource: "workspace source path",
                    requested: abs_path_capacity,
                    unit: "bytes",
                    source: allocation,
                })?;
            path.push(paths.project_root());
            path.push(relative_path.as_ref());
            path
        }
    };
    let name = clone_precharged_string(name, "workspace source name")?;
    let guid = guid
        .map(|guid| clone_precharged_string(guid, "workspace source GUID"))
        .transpose()?;
    let digest = source.fingerprint().digest();
    let kind = search_kind_for_path(Path::new(relative_path.as_ref()));
    let relative_path = match relative_path {
        Cow::Borrowed(relative_path) => {
            clone_precharged_string(relative_path, "workspace source relative path")?
        }
        Cow::Owned(relative_path) => relative_path,
    };
    Ok(ReadSource {
        coordinate,
        rel_path: relative_path,
        abs_path,
        name,
        kind,
        guid,
        bytes: None,
        meta_bytes: None,
        length,
        content_identity: digest,
        hints: SourceHints {
            asset: FileHint {
                size: length,
                mtime_ms: None,
            },
            meta: None,
        },
        unchanged: false,
    })
}

fn cached_workspace_read_source(
    paths: &IndexPaths,
    view: &dyn WorkspaceView,
    cached: &AssetAnalysis,
    budget: &mut AssetLoadBudget,
) -> Result<ReadSource, PipelineError> {
    let root = cached
        .source
        .workspace_source
        .ok_or(PipelineError::Invariant(
            "workspace fallback analysis has no root source",
        ))?;
    let descriptor = match view.source(root, budget)? {
        WorkspaceLookup::Resolved(source) => source,
        WorkspaceLookup::Unloaded
        | WorkspaceLookup::Missing
        | WorkspaceLookup::Ambiguous { .. }
        | WorkspaceLookup::Invalid { .. } => {
            return Err(PipelineError::CachedWorkspaceSourceUnavailable(root));
        }
    };
    let length = view.source_length(root)?;
    let mut source = workspace_read_source(
        paths,
        &descriptor,
        Cow::Borrowed(&cached.source.relative_path),
        length,
        cached.source.guid.as_deref(),
        budget,
    )?;
    source.kind = cached.source.search_kind;
    source.content_identity = cached.source.content_digest;
    source.unchanged = true;
    Ok(source)
}

fn source_scan_hint_precharged(source: &ReadSource) -> Result<SourceScanHint, PipelineError> {
    Ok(SourceScanHint {
        coordinate: source.coordinate,
        relative_path: clone_precharged_string(&source.rel_path, "source scan hint path")?,
        source_length: source.hints.asset.size,
        source_modified_unix_ms: source.hints.asset.mtime_ms,
        metadata_length: source.hints.meta.map(|hint| hint.size),
        metadata_modified_unix_ms: source.hints.meta.and_then(|hint| hint.mtime_ms),
    })
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

fn apply_source_scan_diagnostics(
    analysis: &mut AssetAnalysis,
    diagnostics: &[ScanDiagnostic],
    metrics: &mut AnalysisMetrics,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    let diagnostics_before = analysis.diagnostics.len();
    let truncations_before = analysis.truncations.len();

    for diagnostic in diagnostics {
        match diagnostic {
            ScanDiagnostic::LimitExceeded {
                rel_path,
                part,
                observed_at_least,
                limit,
            } => {
                validate_scan_diagnostic_path(analysis, rel_path)?;
                let (code, label, kind) = match part {
                    SourcePart::Asset => (
                        "SEARCH_SOURCE_ASSET_LIMIT_EXCEEDED",
                        "asset payload",
                        AnalysisTruncationKind::SourceAssetBytes,
                    ),
                    SourcePart::Meta => (
                        "SEARCH_SOURCE_META_LIMIT_EXCEEDED",
                        "metadata payload",
                        AnalysisTruncationKind::SourceMetaBytes,
                    ),
                };
                let message_bytes = bounded_format_len(
                    format_args!(
                        "{label} for {} exceeded the scan limit of {limit} bytes \
                         (observed at least {observed_at_least} bytes); only Tier-0 identity \
                         fields were indexed",
                        analysis.source.relative_path
                    ),
                    2_048,
                    "scan limit diagnostic",
                )?;
                prepare_analysis_annotation(analysis, code, message_bytes, true, budget)?;
                let message = bounded_format_precharged(
                    format_args!(
                        "{label} for {} exceeded the scan limit of {limit} bytes \
                         (observed at least {observed_at_least} bytes); only Tier-0 identity \
                         fields were indexed",
                        analysis.source.relative_path
                    ),
                    message_bytes,
                    "scan limit diagnostic",
                )?;
                analysis.record_incomplete(
                    Diagnostic::new(
                        DiagnosticSeverity::Warning,
                        clone_precharged_string(code, "analysis diagnostic code")?,
                        message,
                    )?,
                    AnalysisTruncation::new(kind, Some(*limit), *observed_at_least),
                );
            }
            ScanDiagnostic::MalformedGuid { rel_path } => {
                validate_scan_diagnostic_path(analysis, rel_path)?;
                let code = "SEARCH_SOURCE_META_GUID_MALFORMED";
                let message_bytes = bounded_format_len(
                    format_args!(
                        "metadata GUID is malformed for {}; GUID-based lookups are unavailable",
                        analysis.source.relative_path
                    ),
                    2_048,
                    "malformed GUID diagnostic",
                )?;
                prepare_analysis_annotation(analysis, code, message_bytes, false, budget)?;
                let message = bounded_format_precharged(
                    format_args!(
                        "metadata GUID is malformed for {}; GUID-based lookups are unavailable",
                        analysis.source.relative_path
                    ),
                    message_bytes,
                    "malformed GUID diagnostic",
                )?;
                analysis.diagnostics.push(Diagnostic::new(
                    DiagnosticSeverity::Warning,
                    clone_precharged_string(code, "analysis diagnostic code")?,
                    message,
                )?);
                analysis.diagnostics.sort_unstable();
                analysis.diagnostics.dedup();
            }
            ScanDiagnostic::PayloadNotRetained { rel_path, .. } => {
                validate_scan_diagnostic_path(analysis, rel_path)?;
                // AssetAnalyzer records the canonical PayloadUnavailable diagnostic and
                // truncation whenever the scanner deliberately omits retained payload bytes.
            }
            ScanDiagnostic::WalkFailed { .. }
            | ScanDiagnostic::PathRejected { .. }
            | ScanDiagnostic::ReadFailed { .. }
            | ScanDiagnostic::AllocationFailed { .. }
            | ScanDiagnostic::BudgetExceeded { .. }
            | ScanDiagnostic::ChangedDuringRead { .. }
            | ScanDiagnostic::DigestFailed { .. } => {
                return Err(PipelineError::Invariant(
                    "scanner attached a rejecting diagnostic to an accepted source",
                ));
            }
        }
    }

    metrics.diagnostics_emitted =
        metrics
            .diagnostics_emitted
            .saturating_add(saturating_usize_to_u64(
                analysis
                    .diagnostics
                    .len()
                    .saturating_sub(diagnostics_before),
            ));
    metrics.truncations_emitted =
        metrics
            .truncations_emitted
            .saturating_add(saturating_usize_to_u64(
                analysis
                    .truncations
                    .len()
                    .saturating_sub(truncations_before),
            ));
    Ok(())
}

fn validate_scan_diagnostic_path(
    analysis: &AssetAnalysis,
    diagnostic_path: &str,
) -> Result<(), PipelineError> {
    if analysis.source.relative_path == diagnostic_path {
        Ok(())
    } else {
        Err(PipelineError::Invariant(
            "scanner diagnostic path does not match analyzed source",
        ))
    }
}

fn mark_workspace_parse_failure(
    analysis: &mut AssetAnalysis,
    message: &str,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    let message = bounded_str(message, 1_024);
    let code = "SEARCH_WORKSPACE_PARSE_FAILED";
    let message_bytes = bounded_format_len(
        format_args!(
            "workspace parsing failed for {}; reference facts are incomplete: {message}",
            analysis.source.relative_path
        ),
        2_048,
        "workspace parse diagnostic",
    )?;
    prepare_analysis_annotation(analysis, code, message_bytes, true, budget)?;
    let diagnostic_message = bounded_format_precharged(
        format_args!(
            "workspace parsing failed for {}; reference facts are incomplete: {message}",
            analysis.source.relative_path
        ),
        message_bytes,
        "workspace parse diagnostic",
    )?;
    let diagnostic = Diagnostic::new(
        DiagnosticSeverity::Warning,
        clone_precharged_string(code, "analysis diagnostic code")?,
        diagnostic_message,
    )?;
    analysis.record_incomplete(
        diagnostic,
        AnalysisTruncation::new(
            AnalysisTruncationKind::WorkspaceParseFailure,
            None,
            analysis.source.length,
        ),
    );
    Ok(())
}

fn bounded_str(message: &str, maximum_bytes: usize) -> &str {
    let mut end = maximum_bytes;
    end = end.min(message.len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

fn is_workspace_candidate(
    source: &ReadSource,
    budget: &AssetLoadBudget,
) -> Result<bool, PipelineError> {
    let Some(bytes) = source.bytes.as_ref() else {
        return Ok(false);
    };
    bytes.validate_budget(budget)?;
    let recognition = recognize_source(Path::new(&source.rel_path), bytes.as_bytes());
    Ok(recognition.is_candidate() && !recognition.is_streamed_resource())
}

fn search_kind_for_path(path: &Path) -> SearchKind {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return SearchKind::File;
    };
    if extension.eq_ignore_ascii_case("prefab") {
        SearchKind::Prefab
    } else if extension.eq_ignore_ascii_case("unity") {
        SearchKind::Scene
    } else if extension.eq_ignore_ascii_case("mat") {
        SearchKind::Material
    } else if extension.eq_ignore_ascii_case("cs") {
        SearchKind::Script
    } else if extension.eq_ignore_ascii_case("anim") {
        SearchKind::AnimationClip
    } else if extension.eq_ignore_ascii_case("controller") {
        SearchKind::AnimatorController
    } else if extension.eq_ignore_ascii_case("asset") {
        SearchKind::Asset
    } else if extension.eq_ignore_ascii_case("shader") {
        SearchKind::Shader
    } else if ["png", "jpg", "jpeg", "tga", "psd"]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    {
        SearchKind::Texture
    } else if ["wav", "mp3", "ogg"]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    {
        SearchKind::Audio
    } else {
        SearchKind::File
    }
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

fn saturating_usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn scan_diagnostics_are_retryable(diagnostics: &[ScanDiagnostic]) -> bool {
    !diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic,
            ScanDiagnostic::PathRejected {
                reason: PathRejection::UnsupportedCaseSensitiveDirectory,
                ..
            }
        )
    })
}

#[derive(Debug)]
pub(crate) enum PipelineError {
    FilesystemReindexRequired,
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    ViewRevisionMismatch {
        expected: WorkspaceRevision,
        actual: WorkspaceRevision,
    },
    RevisionBarrierMismatch {
        indexed: WorkspaceRevision,
        change_from: WorkspaceRevision,
        change_to: WorkspaceRevision,
    },
    TransactionConflict {
        transaction: TransactionId,
    },
    SourceHierarchyCycle(SourceId),
    UnknownWorkspaceSource(SourceId),
    CachedWorkspaceSourceUnavailable(SourceId),
    RelativePathCollision {
        relative_path: String,
        first: SourceId,
        second: SourceId,
    },
    NonPortableWorkspacePath(PathBuf),
    ScanPlanRejected {
        diagnostics: Vec<ScanDiagnostic>,
    },
    ScanRequestRejected {
        diagnostics: Vec<ScanDiagnostic>,
    },
    SourceReadRejected {
        relative_path: String,
        diagnostics: Vec<ScanDiagnostic>,
    },
    GenerationSummaryMismatch {
        resource: &'static str,
        manifest: u64,
        actual: u64,
    },
    Allocation {
        resource: &'static str,
        requested: usize,
        unit: &'static str,
        source: TryReserveError,
    },
    ArithmeticOverflow(&'static str),
    Invariant(&'static str),
    Configuration(anyhow::Error),
    Scan(Box<ScanError>),
    Query(anyhow::Error),
    Projection(anyhow::Error),
    Budget(BudgetError),
    Contract(ContractError),
    Diagnostic(DiagnosticError),
    Workspace(Box<WorkspaceError>),
    SourceAdmission(Box<SourceAdmissionError>),
    ReferenceGraph(Box<ReferenceGraphError>),
    Analysis(Box<AnalysisError>),
    SourceState(Box<SourceStateError>),
    Store(Box<GenerationStoreError>),
    StagingAbortFailed {
        primary: Box<PipelineError>,
        cleanup: Box<GenerationStoreError>,
    },
    Manifest(GenerationManifestError),
    Json(serde_json::Error),
}

impl PipelineError {
    pub(crate) fn api_code(&self) -> ApiErrorCode {
        match self {
            Self::StagingAbortFailed { primary, .. } => primary.api_code(),
            Self::FilesystemReindexRequired => ApiErrorCode::NotReady,
            Self::Configuration(_)
            | Self::Contract(_)
            | Self::Diagnostic(_)
            | Self::NonPortableWorkspacePath(_)
            | Self::RelativePathCollision { .. }
            | Self::ScanRequestRejected { .. }
            | Self::TransactionConflict { .. } => ApiErrorCode::InvalidRequest,
            Self::WorkspaceMismatch { .. }
            | Self::ViewRevisionMismatch { .. }
            | Self::RevisionBarrierMismatch { .. } => ApiErrorCode::RevisionMismatch,
            Self::Store(error)
                if matches!(
                    error.as_ref(),
                    GenerationStoreError::WriterLeaseUnavailable { .. }
                ) =>
            {
                ApiErrorCode::Busy
            }
            Self::SourceHierarchyCycle(_)
            | Self::UnknownWorkspaceSource(_)
            | Self::CachedWorkspaceSourceUnavailable(_)
            | Self::ScanPlanRejected { .. }
            | Self::SourceReadRejected { .. }
            | Self::GenerationSummaryMismatch { .. }
            | Self::Allocation { .. }
            | Self::ArithmeticOverflow(_)
            | Self::Invariant(_)
            | Self::Scan(_)
            | Self::Query(_)
            | Self::Projection(_)
            | Self::Budget(_)
            | Self::Workspace(_)
            | Self::SourceAdmission(_)
            | Self::ReferenceGraph(_)
            | Self::Analysis(_)
            | Self::SourceState(_)
            | Self::Store(_)
            | Self::Manifest(_)
            | Self::Json(_) => ApiErrorCode::IndexBuildFailed,
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        match self {
            Self::StagingAbortFailed { primary, cleanup } => {
                primary.retryable() || cleanup.is_retryable()
            }
            Self::FilesystemReindexRequired => true,
            Self::ScanPlanRejected { diagnostics }
            | Self::SourceReadRejected { diagnostics, .. } => {
                scan_diagnostics_are_retryable(diagnostics)
            }
            Self::Scan(error) => error.retryable(),
            Self::SourceAdmission(error) => matches!(
                error.category(),
                SourceAdmissionErrorCategory::Io | SourceAdmissionErrorCategory::SourceChanged
            ),
            Self::Workspace(error) => matches!(
                error.as_ref(),
                WorkspaceError::Io { .. } | WorkspaceError::SourceChanged { .. }
            ),
            Self::Store(error) => error.is_retryable(),
            Self::WorkspaceMismatch { .. }
            | Self::ViewRevisionMismatch { .. }
            | Self::RevisionBarrierMismatch { .. }
            | Self::TransactionConflict { .. }
            | Self::SourceHierarchyCycle(_)
            | Self::UnknownWorkspaceSource(_)
            | Self::CachedWorkspaceSourceUnavailable(_)
            | Self::RelativePathCollision { .. }
            | Self::NonPortableWorkspacePath(_)
            | Self::ScanRequestRejected { .. }
            | Self::GenerationSummaryMismatch { .. }
            | Self::Allocation { .. }
            | Self::ArithmeticOverflow(_)
            | Self::Invariant(_)
            | Self::Configuration(_)
            | Self::Query(_)
            | Self::Projection(_)
            | Self::Budget(_)
            | Self::Contract(_)
            | Self::Diagnostic(_)
            | Self::ReferenceGraph(_)
            | Self::Analysis(_)
            | Self::SourceState(_)
            | Self::Manifest(_)
            | Self::Json(_) => false,
        }
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FilesystemReindexRequired => formatter.write_str(
                "the active generation uses incompatible persisted semantics; run a full filesystem reindex before applying workspace changes",
            ),
            Self::WorkspaceMismatch { expected, actual } => write!(
                formatter,
                "workspace {actual:?} does not match indexed workspace {expected:?}"
            ),
            Self::ViewRevisionMismatch { expected, actual } => write!(
                formatter,
                "workspace view revision {actual} does not match ChangeSet target {expected}"
            ),
            Self::RevisionBarrierMismatch {
                indexed,
                change_from,
                change_to,
            } => write!(
                formatter,
                "indexed revision {indexed} cannot apply ChangeSet {change_from} -> {change_to}"
            ),
            Self::TransactionConflict { transaction } => write!(
                formatter,
                "transaction {transaction} conflicts with its persisted ChangeSet receipt"
            ),
            Self::SourceHierarchyCycle(source) => {
                write!(formatter, "workspace source hierarchy cycles at {source:?}")
            }
            Self::UnknownWorkspaceSource(source) => {
                write!(
                    formatter,
                    "workspace source hierarchy is missing {source:?}"
                )
            }
            Self::CachedWorkspaceSourceUnavailable(source) => write!(
                formatter,
                "cached workspace source {source:?} is unavailable in the current view"
            ),
            Self::RelativePathCollision {
                relative_path,
                first,
                second,
            } => write!(
                formatter,
                "workspace roots {first:?} and {second:?} map to the same search path {relative_path:?}"
            ),
            Self::NonPortableWorkspacePath(path) => write!(
                formatter,
                "workspace physical origin has no portable project-relative path: {}",
                path.display()
            ),
            Self::ScanPlanRejected { diagnostics } => write!(
                formatter,
                "project discovery failed with {} diagnostic(s)",
                diagnostics.len()
            ),
            Self::ScanRequestRejected { diagnostics } => write!(
                formatter,
                "changed-path request contained {} rejected path diagnostic(s)",
                diagnostics.len()
            ),
            Self::SourceReadRejected {
                relative_path,
                diagnostics,
            } => write!(
                formatter,
                "source {relative_path:?} could not be read once and completely ({} diagnostic(s))",
                diagnostics.len()
            ),
            Self::GenerationSummaryMismatch {
                resource,
                manifest,
                actual,
            } => write!(
                formatter,
                "generation projection summary for {resource} is {manifest}, but readable artifacts contain {actual}"
            ),
            Self::Allocation {
                resource,
                requested,
                unit,
                ..
            } => write!(
                formatter,
                "failed to reserve {requested} {unit} for pipeline {resource}"
            ),
            Self::ArithmeticOverflow(resource) => {
                write!(formatter, "pipeline arithmetic overflow for {resource}")
            }
            Self::Invariant(message) => write!(formatter, "pipeline invariant failed: {message}"),
            Self::Configuration(error)
            | Self::Query(error)
            | Self::Projection(error) => fmt::Display::fmt(error, formatter),
            Self::Scan(error) => fmt::Display::fmt(error, formatter),
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::Contract(error) => fmt::Display::fmt(error, formatter),
            Self::Diagnostic(error) => fmt::Display::fmt(error, formatter),
            Self::Workspace(error) => fmt::Display::fmt(error, formatter),
            Self::SourceAdmission(error) => fmt::Display::fmt(error, formatter),
            Self::ReferenceGraph(error) => fmt::Display::fmt(error, formatter),
            Self::Analysis(error) => fmt::Display::fmt(error, formatter),
            Self::SourceState(error) => fmt::Display::fmt(error, formatter),
            Self::Store(error) => fmt::Display::fmt(error, formatter),
            Self::StagingAbortFailed { primary, cleanup } => write!(
                formatter,
                "{primary}; generation staging cleanup also failed: {cleanup}"
            ),
            Self::Manifest(error) => fmt::Display::fmt(error, formatter),
            Self::Json(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl StdError for PipelineError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Configuration(error) | Self::Query(error) | Self::Projection(error) => {
                Some(error.as_ref())
            }
            Self::Scan(error) => Some(error.as_ref()),
            Self::Budget(error) => Some(error),
            Self::Contract(error) => Some(error),
            Self::Diagnostic(error) => Some(error),
            Self::Workspace(error) => Some(error.as_ref()),
            Self::SourceAdmission(error) => Some(error.as_ref()),
            Self::ReferenceGraph(error) => Some(error.as_ref()),
            Self::Analysis(error) => Some(error.as_ref()),
            Self::SourceState(error) => Some(error.as_ref()),
            Self::Store(error) => Some(error.as_ref()),
            Self::StagingAbortFailed { primary, .. } => Some(primary.as_ref()),
            Self::Manifest(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Allocation { source, .. } => Some(source),
            Self::FilesystemReindexRequired
            | Self::WorkspaceMismatch { .. }
            | Self::ViewRevisionMismatch { .. }
            | Self::RevisionBarrierMismatch { .. }
            | Self::TransactionConflict { .. }
            | Self::SourceHierarchyCycle(_)
            | Self::UnknownWorkspaceSource(_)
            | Self::CachedWorkspaceSourceUnavailable(_)
            | Self::RelativePathCollision { .. }
            | Self::NonPortableWorkspacePath(_)
            | Self::ScanPlanRejected { .. }
            | Self::ScanRequestRejected { .. }
            | Self::SourceReadRejected { .. }
            | Self::GenerationSummaryMismatch { .. }
            | Self::ArithmeticOverflow(_)
            | Self::Invariant(_) => None,
        }
    }
}

impl From<BudgetError> for PipelineError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<ContractError> for PipelineError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

impl From<DiagnosticError> for PipelineError {
    fn from(error: DiagnosticError) -> Self {
        Self::Diagnostic(error)
    }
}

impl From<WorkspaceError> for PipelineError {
    fn from(error: WorkspaceError) -> Self {
        Self::Workspace(Box::new(error))
    }
}

impl From<SourceAdmissionError> for PipelineError {
    fn from(error: SourceAdmissionError) -> Self {
        Self::SourceAdmission(Box::new(error))
    }
}

impl From<SourceAdmissionBatchAllocationError> for PipelineError {
    fn from(error: SourceAdmissionBatchAllocationError) -> Self {
        match error {
            SourceAdmissionBatchAllocationError::Budget(error) => Self::Budget(error),
            SourceAdmissionBatchAllocationError::Allocation { requested, source } => {
                Self::Allocation {
                    resource: "filesystem source admission batch",
                    requested,
                    unit: "operations",
                    source,
                }
            }
        }
    }
}

impl From<SourceAdmissionBatchPushError> for PipelineError {
    fn from(error: SourceAdmissionBatchPushError) -> Self {
        match error {
            SourceAdmissionBatchPushError::Budget(error) => Self::Budget(error),
            SourceAdmissionBatchPushError::Capacity(_) => Self::Invariant(
                "filesystem source admission count exceeded its exact planned capacity",
            ),
        }
    }
}

impl From<ReferenceGraphError> for PipelineError {
    fn from(error: ReferenceGraphError) -> Self {
        Self::ReferenceGraph(Box::new(error))
    }
}

impl From<AnalysisError> for PipelineError {
    fn from(error: AnalysisError) -> Self {
        Self::Analysis(Box::new(error))
    }
}

impl From<SourceStateError> for PipelineError {
    fn from(error: SourceStateError) -> Self {
        Self::SourceState(Box::new(error))
    }
}

impl From<GenerationStoreError> for PipelineError {
    fn from(error: GenerationStoreError) -> Self {
        Self::Store(Box::new(error))
    }
}

impl From<GenerationManifestError> for PipelineError {
    fn from(error: GenerationManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<serde_json::Error> for PipelineError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectPathSpace;
    use crate::analysis::{AnalyzedSource, SearchFacts};
    use crate::anchored_fs::AnchoredFsError;
    use crate::scan::ScanReadLimits;
    use unity_asset::{AssetLoadLimits, BudgetedSourceBytes, SourceKind};
    use unity_asset_search_protocol::ProjectId;

    fn test_coordinate(path: &str) -> IndexedSourceCoordinate {
        #[cfg(windows)]
        let root = PathBuf::from(r"C:\Project");
        #[cfg(not(windows))]
        let root = PathBuf::from("/Project");
        let space = ProjectPathSpace::new(root, ProjectId::from_bytes([9; 32])).unwrap();
        IndexedSourceCoordinate::project(
            space.resolve(Path::new(path)).unwrap().unwrap().identity(),
        )
    }

    fn write_obsolete_v2_activation(
        paths: &IndexPaths,
        workspace: WorkspaceId,
        actual_revision: WorkspaceRevision,
        desired_revision: WorkspaceRevision,
    ) {
        let activation = serde_json::json!({
            "contract_version": 2,
            "ordinal": 1,
            "generation": crate::generation::SearchGenerationId::new(DigestV1::hash_bytes(
                b"obsolete-v2-generation",
            )),
            "manifest_digest": DigestV1::hash_bytes(b"obsolete-v2-manifest"),
            "workspace": workspace,
            "revision": actual_revision,
            "desired_revision": desired_revision,
        });
        std::fs::write(
            paths
                .index_root()
                .join("activations")
                .join("00000000000000000001.json"),
            serde_json::to_vec(&activation).unwrap(),
        )
        .unwrap();
    }

    fn tier_zero_analysis(path: &str) -> AssetAnalysis {
        AssetAnalysis::new(
            AnalyzedSource {
                coordinate: test_coordinate(path),
                relative_path: path.to_owned(),
                content_digest: DigestV1::hash_bytes(path.as_bytes()),
                length: 32,
                search_kind: SearchKind::File,
                guid: None,
                workspace_source: None,
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

    #[cfg(windows)]
    #[test]
    fn logical_workspace_roots_do_not_inherit_windows_path_equivalence() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let first = SourceId::new(workspace, SourceKind::Yaml, 1).unwrap();
        let second = SourceId::new(workspace, SourceKind::Yaml, 2).unwrap();
        let mut roots = vec![
            (
                IndexedSourceCoordinate::workspace(first),
                "Assets/Hero".to_owned(),
                first,
            ),
            (
                IndexedSourceCoordinate::workspace(second),
                "assets/HERO".to_owned(),
                second,
            ),
        ];

        sort_workspace_root_paths(&mut roots).unwrap();

        assert_eq!(roots.len(), 2);
        assert_ne!(roots[0].0, roots[1].0);
    }

    #[test]
    fn workspace_sniff_rejects_source_bytes_from_another_budget_domain() {
        let mut source_budget = AssetLoadBudget::default();
        let source = test_read_source("Assets/Unknown.data", b"%YAML 1.1\n", &mut source_budget);

        assert!(is_workspace_candidate(&source, &source_budget).unwrap());
        let error = is_workspace_candidate(&source, &AssetLoadBudget::default()).unwrap_err();
        assert!(matches!(
            error,
            PipelineError::Budget(BudgetError::DomainMismatch {
                resource: "source bytes"
            })
        ));
    }

    #[test]
    fn workspace_candidates_reuse_the_workspace_recognition_matrix() {
        for (path, bytes, expected) in [
            ("Assets/material.mat", b"".as_slice(), true),
            ("Assets/clip.anim", b"", true),
            ("Assets/state.controller", b"", true),
            ("Assets/archive.bin", b"PK\x06\x06", true),
            ("Assets/not-an-archive.bin", b"PK\x07\x08", false),
            ("Assets/CAB-data.resS", b"", false),
        ] {
            let mut budget = AssetLoadBudget::default();
            let source = test_read_source(path, bytes, &mut budget);

            assert_eq!(
                is_workspace_candidate(&source, &budget).unwrap(),
                expected,
                "unexpected workspace-candidate decision for {path}"
            );
        }
    }

    fn test_read_source(path: &str, bytes: &[u8], budget: &mut AssetLoadBudget) -> ReadSource {
        let length = u64::try_from(bytes.len()).unwrap();
        ReadSource {
            coordinate: test_coordinate(path),
            rel_path: path.to_owned(),
            abs_path: PathBuf::from(path),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            kind: search_kind_for_path(Path::new(path)),
            guid: None,
            bytes: Some(BudgetedSourceBytes::from_vec(bytes.to_vec(), budget).unwrap()),
            meta_bytes: None,
            length,
            content_identity: DigestV1::hash_bytes(bytes),
            hints: SourceHints {
                asset: FileHint {
                    size: length,
                    mtime_ms: None,
                },
                meta: None,
            },
            unchanged: false,
        }
    }

    #[test]
    fn scan_limits_record_precise_incomplete_analysis() {
        for (part, expected_kind, expected_code) in [
            (
                SourcePart::Asset,
                AnalysisTruncationKind::SourceAssetBytes,
                "SEARCH_SOURCE_ASSET_LIMIT_EXCEEDED",
            ),
            (
                SourcePart::Meta,
                AnalysisTruncationKind::SourceMetaBytes,
                "SEARCH_SOURCE_META_LIMIT_EXCEEDED",
            ),
        ] {
            let mut analysis = tier_zero_analysis("Assets/Large.asset");
            let mut metrics = AnalysisMetrics::default();
            let mut budget = AssetLoadBudget::default();
            apply_source_scan_diagnostics(
                &mut analysis,
                &[ScanDiagnostic::LimitExceeded {
                    rel_path: "Assets/Large.asset".to_owned(),
                    part,
                    observed_at_least: 101,
                    limit: 100,
                }],
                &mut metrics,
                &mut budget,
            )
            .unwrap();

            assert!(!analysis.complete);
            assert_eq!(analysis.truncations.len(), 1);
            assert_eq!(analysis.truncations[0].kind, expected_kind);
            assert_eq!(analysis.truncations[0].limit, Some(100));
            assert_eq!(analysis.truncations[0].observed_at_least, 101);
            assert_eq!(analysis.diagnostics.len(), 1);
            assert_eq!(analysis.diagnostics[0].code(), expected_code);
            assert_eq!(metrics.truncations_emitted, 1);
            assert_eq!(metrics.diagnostics_emitted, 1);
        }
    }

    #[test]
    fn scan_diagnostic_cannot_cross_asset_boundaries() {
        let mut analysis = tier_zero_analysis("Assets/One.asset");
        let mut metrics = AnalysisMetrics::default();
        let mut budget = AssetLoadBudget::default();
        let error = apply_source_scan_diagnostics(
            &mut analysis,
            &[ScanDiagnostic::LimitExceeded {
                rel_path: "Assets/Two.asset".to_owned(),
                part: SourcePart::Asset,
                observed_at_least: 101,
                limit: 100,
            }],
            &mut metrics,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(error, PipelineError::Invariant(_)));
        assert!(analysis.complete);
        assert!(analysis.diagnostics.is_empty());
        assert!(analysis.truncations.is_empty());
    }

    #[test]
    fn unsupported_case_sensitive_directories_are_not_retryable() {
        let scan_error = PipelineError::Scan(Box::new(ScanError::TraversalRead {
            source: AnchoredFsError::UnsupportedCaseSensitiveDirectory,
        }));
        let diagnostic_error = PipelineError::SourceReadRejected {
            relative_path: "Assets/CaseSensitive.asset".to_owned(),
            diagnostics: vec![ScanDiagnostic::PathRejected {
                path: PathBuf::from("Assets/CaseSensitive.asset"),
                reason: PathRejection::UnsupportedCaseSensitiveDirectory,
            }],
        };

        assert!(!scan_error.retryable());
        assert!(!diagnostic_error.retryable());
    }

    #[test]
    fn workspace_reindex_discards_scan_hints_for_removed_assets() {
        let assets = [
            tier_zero_analysis("Assets/A.asset"),
            tier_zero_analysis("Assets/C.asset"),
        ];
        let mut scan_hints = ["Assets/A.asset", "Assets/B.asset", "Assets/C.asset"]
            .into_iter()
            .map(|path| {
                SourceScanHint::new(test_coordinate(path), path.to_owned(), 32, None, None, None)
                    .unwrap()
            })
            .collect();

        retain_scan_hints_for_assets(&mut scan_hints, &assets);

        assert_eq!(
            scan_hints
                .iter()
                .map(|hint| hint.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["Assets/A.asset", "Assets/C.asset"]
        );
    }

    #[test]
    fn filesystem_reindex_deletes_project_assets_without_scan_hints() {
        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project_root,
            Some(temporary.path().join("index")),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let coordinate = IndexedSourceCoordinate::project(
            paths
                .project_path_space()
                .resolve(Path::new("Assets/Removed.asset"))
                .unwrap()
                .unwrap()
                .identity(),
        );
        let mut analysis = tier_zero_analysis("Assets/Removed.asset");
        analysis.source.coordinate = coordinate;
        let state = SourceStateSnapshot::new(
            WorkspaceId::from_u128(1).unwrap(),
            WorkspaceRevision::new(DigestV1::hash_bytes(b"revision")),
            Vec::new(),
            vec![analysis],
        )
        .unwrap();
        let known = clone_known_paths(Some(&state), &mut AssetLoadBudget::default()).unwrap();
        let scanner = ProjectScanner::new(
            &paths,
            SearchIndexOptions::default(),
            ScanReadLimits::default(),
        )
        .unwrap();

        for intent in [ScanIntent::Full, ScanIntent::Reconcile] {
            let plan = scanner
                .plan(intent, &known, &mut AssetLoadBudget::default())
                .unwrap();

            assert!(plan.present.is_empty());
            assert_eq!(plan.deleted.len(), 1);
            assert_eq!(plan.deleted[0].relative_path(), "Assets/Removed.asset");
            assert_eq!(
                plan.deleted[0].identity(),
                coordinate.project_path().unwrap()
            );
        }
    }

    #[test]
    fn retained_vec_accepts_exact_budget_and_rejects_one_byte_less() {
        let retained_bytes = vec_allocation_bytes::<u32>(3).unwrap();
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 3,
            max_members: 3,
            max_bytes: retained_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let values = reserve_retained_vec::<u32>(3, "test vector", &mut exact).unwrap();

        assert!(values.capacity() >= 3);
        assert_eq!(exact.usage().entries, 3);
        assert_eq!(exact.usage().members, 3);
        assert_eq!(exact.usage().bytes, retained_bytes);

        let mut low = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 3,
            max_members: 3,
            max_bytes: retained_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = reserve_retained_vec::<u32>(3, "test vector", &mut low).unwrap_err();

        assert!(matches!(
            error,
            PipelineError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(low.usage().entries, 0);
        assert_eq!(low.usage().members, 0);
        assert_eq!(low.usage().bytes, 0);

        let mut low_members = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 3,
            max_members: 2,
            max_bytes: retained_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = reserve_retained_vec::<u32>(3, "test vector", &mut low_members).unwrap_err();

        assert!(matches!(
            error,
            PipelineError::Budget(BudgetError::Exceeded {
                resource: "members",
                ..
            })
        ));
        assert_eq!(low_members.usage().entries, 0);
        assert_eq!(low_members.usage().members, 0);
        assert_eq!(low_members.usage().bytes, 0);
    }

    #[test]
    fn retained_string_rejects_before_materialization() {
        let mut low = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 4,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = charge_retained_string("12345", "test string", &mut low).unwrap_err();

        assert!(matches!(
            error,
            PipelineError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(low.usage().bytes, 0);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 5,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        charge_retained_string("12345", "test string", &mut exact).unwrap();
        let value = clone_precharged_string("12345", "test string").unwrap();

        assert_eq!(value, "12345");
        assert_eq!(exact.usage().bytes, 5);
    }

    #[test]
    fn no_change_returns_pending_committed_head_warnings() {
        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project_root,
            Some(temporary.path().join("index")),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let mut pipeline = SearchGenerationPipeline::open(
            paths,
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let first = pipeline
            .reindex_filesystem(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(first.disposition, PipelineBuildDisposition::Published);

        pipeline.append_pending_publish_warnings([GenerationPublishWarning::new(
            GenerationPublishWarningKind::PostCommitDurability,
            "committed head warning",
        )]);
        assert_eq!(
            pipeline.generation_maintenance().state,
            GenerationMaintenanceState::RecoveryRequired
        );
        let unchanged = pipeline
            .reindex_filesystem(
                FilesystemReindexIntent::reconcile(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(unchanged.disposition, PipelineBuildDisposition::NoChange);
        assert_eq!(unchanged.warnings, ["committed head warning"]);
        assert!(pipeline.pending_publish_warnings.is_empty());
    }

    #[test]
    fn semantic_mismatch_keeps_the_old_generation_queryable_and_forces_a_full_rebuild() {
        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project_root,
            Some(temporary.path().join("index")),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let mut current = SearchGenerationPipeline::open(
            paths.clone(),
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let published = current
            .reindex_filesystem(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let old_generation = published.active.unwrap().snapshot.generation();
        drop(current);

        let changed_semantics = SearchSemantics::current()
            .with_reference_projection_digest(DigestV1::hash_bytes(b"reference projection v-next"));
        let mut reopened = SearchGenerationPipeline::open_with_semantics(
            paths,
            SearchIndexOptions::default(),
            changed_semantics,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let stale = reopened.active.as_ref().unwrap();
        assert_eq!(stale.snapshot.generation(), old_generation);
        assert!(!stale.stamp.semantics_current);
        assert!(stale.stamp.configuration_current);
        assert!(stale.stamp.stale);
        assert!(!reopened.active_analysis_match);
        assert!(!reopened.active_compatibility_match);

        let rebuilt = reopened
            .reindex_filesystem(
                FilesystemReindexIntent::reconcile(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(rebuilt.disposition, PipelineBuildDisposition::Published);
        assert!(rebuilt.metrics.forced_full_analysis);
        let active = rebuilt.active.unwrap();
        assert_ne!(active.snapshot.generation(), old_generation);
        assert!(active.stamp.semantics_current);
        assert!(active.stamp.configuration_current);
        assert!(!active.stamp.stale);
    }

    #[test]
    fn semantic_layout_mismatch_rebuilds_without_decoding_the_obsolete_source_state() {
        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project_root,
            Some(temporary.path().join("index")),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let mut current = SearchGenerationPipeline::open(
            paths.clone(),
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let published = current
            .reindex_filesystem(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let old_generation = published.active.unwrap().snapshot.generation();
        let workspace = current.workspace.workspace_id();
        drop(current);

        let changed_semantics = SearchSemantics::current().with_analysis_version(
            SearchSemantics::current()
                .analysis_version()
                .saturating_add(1),
        );
        let mut reopened = SearchGenerationPipeline::open_with_semantics(
            paths,
            SearchIndexOptions::default(),
            changed_semantics,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert!(reopened.active.is_none());
        assert!(reopened.source_state.is_none());
        assert_eq!(reopened.workspace.workspace_id(), workspace);
        assert!(!reopened.active_analysis_match);
        assert!(!reopened.active_compatibility_match);

        let rebuilt = reopened
            .reindex_filesystem(
                FilesystemReindexIntent::reconcile(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(rebuilt.disposition, PipelineBuildDisposition::Published);
        assert!(rebuilt.metrics.forced_full_analysis);
        let active = rebuilt.active.unwrap();
        assert_ne!(active.snapshot.generation(), old_generation);
        assert!(active.stamp.semantics_current);
    }

    #[test]
    fn obsolete_v2_activation_rebuild_preserves_workspace_transaction_continuity() {
        const SOURCE: &[u8] = b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Bootstrap\n";

        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project_root,
            Some(temporary.path().join("index")),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let workspace_id = WorkspaceId::from_u128(0x9002).unwrap();
        let empty_workspace =
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::lenient()).unwrap();
        let empty_revision = empty_workspace.revision();

        let source_path = temporary.path().join("external.prefab");
        std::fs::write(&source_path, SOURCE).unwrap();
        let mut target_workspace =
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::lenient()).unwrap();
        let mut workspace_budget = AssetLoadBudget::default();
        let source = target_workspace
            .load_source_bytes(
                SourceOpenRequest::new(
                    source_path,
                    SourceAlias::new("external.prefab".to_owned()).unwrap(),
                )
                .with_kind_hint(SourceKind::Yaml),
                Arc::<[u8]>::from(SOURCE),
                &mut workspace_budget,
            )
            .unwrap();
        let target_revision = target_workspace.revision();
        let transaction = TransactionId::new(DigestV1::hash_bytes(b"bootstrap-change"));
        let changes = ChangeSet::new(
            transaction,
            workspace_id,
            empty_revision,
            target_revision,
            vec![source],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        drop(
            SearchGenerationPipeline::open(
                paths.clone(),
                SearchIndexOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        );
        write_obsolete_v2_activation(&paths, workspace_id, empty_revision, target_revision);

        let obsolete_activation = paths
            .index_root()
            .join("activations")
            .join("00000000000000000001.json");
        let mut interrupted = SearchGenerationPipeline::open(
            paths.clone(),
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(interrupted.workspace.workspace_id(), workspace_id);
        assert!(interrupted.active.is_none());
        assert!(interrupted.rebuild_bootstrap.is_some());
        assert!(obsolete_activation.is_file());
        assert!(matches!(
            interrupted.reindex_workspace(
                changes.clone(),
                &target_workspace.snapshot(),
                &mut AssetLoadBudget::default(),
            ),
            Err(PipelineError::FilesystemReindexRequired)
        ));
        drop(interrupted);

        let mut reopened = SearchGenerationPipeline::open(
            paths,
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let bootstrap = reopened.rebuild_bootstrap.as_ref().unwrap();
        assert_eq!(bootstrap.workspace(), workspace_id);
        assert_eq!(bootstrap.actual_revision(), empty_revision);
        assert_eq!(bootstrap.desired_revision(), target_revision);
        assert!(obsolete_activation.is_file());

        let rebuilt = reopened
            .reindex_filesystem(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(rebuilt.disposition, PipelineBuildDisposition::Published);
        let rebuilt = rebuilt.active.unwrap();
        assert_eq!(rebuilt.snapshot.manifest().workspace(), workspace_id);
        assert_eq!(rebuilt.snapshot.manifest().revision(), empty_revision);
        assert_eq!(rebuilt.snapshot.desired_revision(), target_revision);
        assert!(!obsolete_activation.exists());
        assert!(reopened.rebuild_bootstrap.is_none());

        let applied = reopened
            .reindex_workspace(
                changes,
                &target_workspace.snapshot(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(applied.disposition, PipelineBuildDisposition::Published);
        let active = applied.active.unwrap();
        assert_eq!(active.snapshot.manifest().workspace(), workspace_id);
        assert_eq!(active.snapshot.manifest().revision(), target_revision);
        assert_eq!(active.snapshot.desired_revision(), target_revision);
        assert_eq!(
            reopened
                .store
                .active()
                .unwrap()
                .transaction_receipts()
                .ids()
                .collect::<Vec<_>>(),
            [transaction]
        );
    }

    #[test]
    fn obsolete_v2_desired_revision_is_discarded_when_rebuilt_content_changed() {
        const SOURCE: &[u8] = b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Changed\n";

        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project_root.clone(),
            Some(temporary.path().join("index")),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let workspace_id = WorkspaceId::from_u128(0x9003).unwrap();
        let empty_workspace =
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::lenient()).unwrap();
        let obsolete_actual = empty_workspace.revision();
        let obsolete_desired = WorkspaceRevision::new(DigestV1::hash_bytes(b"obsolete-desired"));

        drop(
            SearchGenerationPipeline::open(
                paths.clone(),
                SearchIndexOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        );
        write_obsolete_v2_activation(&paths, workspace_id, obsolete_actual, obsolete_desired);
        std::fs::write(project_root.join("Assets/Changed.prefab"), SOURCE).unwrap();

        let mut pipeline = SearchGenerationPipeline::open(
            paths,
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let rebuilt = pipeline
            .reindex_filesystem(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let active = rebuilt.active.unwrap();
        let rebuilt_revision = active.snapshot.manifest().revision();
        assert_ne!(rebuilt_revision, obsolete_actual);
        assert_eq!(active.snapshot.desired_revision(), rebuilt_revision);
        assert_ne!(active.snapshot.desired_revision(), obsolete_desired);

        let stale_change = ChangeSet::new(
            TransactionId::new(DigestV1::hash_bytes(b"stale-bootstrap-change")),
            workspace_id,
            obsolete_actual,
            obsolete_desired,
            vec![SourceId::new(workspace_id, SourceKind::Yaml, 1).unwrap()],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let error = pipeline
            .reindex_workspace(
                stale_change,
                &empty_workspace.snapshot(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            PipelineError::RevisionBarrierMismatch {
                indexed,
                change_from,
                change_to,
            } if indexed == rebuilt_revision
                && change_from == obsolete_actual
                && change_to == obsolete_desired
        ));
    }

    #[test]
    fn scan_root_change_is_stale_until_a_full_cache_discarding_rebuild() {
        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        std::fs::create_dir_all(project_root.join("Packages")).unwrap();
        std::fs::write(project_root.join("Assets/Current.txt"), b"current").unwrap();
        std::fs::write(project_root.join("Packages/Removed.txt"), b"removed").unwrap();
        let index_root = temporary.path().join("index");
        let all_paths = IndexPaths::for_project(
            project_root.clone(),
            Some(index_root.clone()),
            Some(vec![PathBuf::from("Assets"), PathBuf::from("Packages")]),
        )
        .unwrap();
        let mut current = SearchGenerationPipeline::open(
            all_paths,
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let published = current
            .reindex_filesystem(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let old_generation = published.active.unwrap().snapshot.generation();
        drop(current);

        let assets_only = IndexPaths::for_project(
            project_root,
            Some(index_root),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let mut reopened = SearchGenerationPipeline::open(
            assets_only,
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let stale = reopened.active.as_ref().unwrap();
        assert_eq!(stale.snapshot.generation(), old_generation);
        assert!(stale.stamp.semantics_current);
        assert!(!stale.stamp.configuration_current);
        assert!(stale.stamp.stale);
        assert!(!reopened.active_analysis_match);
        assert!(!reopened.active_compatibility_match);
        assert!(
            reopened
                .source_state
                .as_ref()
                .unwrap()
                .analysis("Packages/Removed.txt")
                .is_some()
        );

        let rebuilt = reopened
            .reindex_filesystem(
                FilesystemReindexIntent::reconcile(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(rebuilt.disposition, PipelineBuildDisposition::Published);
        assert!(rebuilt.metrics.forced_full_scan);
        assert!(rebuilt.metrics.forced_full_analysis);
        assert!(
            reopened
                .source_state
                .as_ref()
                .unwrap()
                .analysis("Packages/Removed.txt")
                .is_none()
        );
        let active = rebuilt.active.unwrap();
        assert_ne!(active.snapshot.generation(), old_generation);
        assert!(active.stamp.semantics_current);
        assert!(active.stamp.configuration_current);
        assert!(!active.stamp.stale);
    }

    #[test]
    fn pending_publish_warnings_are_bounded_with_explicit_omission_evidence() {
        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project_root,
            Some(temporary.path().join("index")),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let mut pipeline = SearchGenerationPipeline::open(
            paths,
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        pipeline.append_pending_publish_warnings((0..MAX_REINDEX_PUBLISH_WARNINGS + 8).map(
            |ordinal| {
                GenerationPublishWarning::new(
                    GenerationPublishWarningKind::PostCommitDurability,
                    format!("warning-{ordinal}:{}", "x".repeat(4 * 1024)),
                )
            },
        ));

        assert!(pipeline.pending_publish_warnings.len() <= MAX_REINDEX_PUBLISH_WARNINGS);
        let warnings = pipeline.take_pending_publish_warnings();

        ReindexEvidence::validate_publish_warnings(&warnings).unwrap();
        assert!(
            warnings
                .last()
                .is_some_and(|warning| warning.contains("were omitted"))
        );
        assert!(pipeline.pending_publish_warnings.is_empty());
        assert!(!pipeline.pending_publish_warnings_omitted);
    }

    #[test]
    fn unknown_activation_outcome_marks_generation_maintenance_recovery_required() {
        let temporary = crate::secure_test_tempdir();
        let project_root = temporary.path().join("project");
        std::fs::create_dir_all(project_root.join("Assets")).unwrap();
        let paths = IndexPaths::for_project(
            project_root,
            Some(temporary.path().join("index")),
            Some(vec![PathBuf::from("Assets")]),
        )
        .unwrap();
        let mut pipeline = SearchGenerationPipeline::open(
            paths,
            SearchIndexOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        pipeline.record_store_reopen_requirement(&GenerationStoreError::ActivationOutcomeUnknown);

        assert_eq!(
            pipeline.generation_maintenance().state,
            GenerationMaintenanceState::RecoveryRequired
        );
        assert!(
            pipeline
                .generation_maintenance()
                .last_cleanup_failure
                .as_deref()
                .is_some_and(|message| message.contains("reopen the generation store"))
        );
    }

    #[test]
    fn nested_generation_cleanup_io_remains_retryable() {
        let error = PipelineError::Store(Box::new(
            GenerationStoreError::ActivationPreCommitCleanupFailed {
                primary: Box::new(GenerationStoreError::InjectedFailure {
                    checkpoint: GenerationFailpoint::ActivationPreCommit,
                }),
                cleanup: Box::new(GenerationStoreError::Io {
                    operation: "remove activation staging file",
                    path: PathBuf::from("activation-staging"),
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "injected cleanup"),
                }),
            },
        ));

        assert_eq!(error.api_code(), ApiErrorCode::IndexBuildFailed);
        assert!(error.retryable());
    }
}
