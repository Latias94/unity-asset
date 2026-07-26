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
    AssetWorkspace, SourceOpenRequest, WorkspaceError, WorkspaceLookup, WorkspaceOptions,
    WorkspaceSource, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, BinaryError, BudgetError, ChangeSet, ContractError, Diagnostic,
    DiagnosticError, DiagnosticSeverity, DigestV1, ObjectAddress, ObjectId, SourceAlias, SourceId,
    SourceLocator, TransactionId, WorkspaceId, WorkspaceRevision,
};
use unity_asset_core::{string_allocation_bytes, vec_allocation_bytes};
use unity_asset_search_core::{SearchKind, SearchRequest};

use crate::analysis::{
    AnalysisMetrics, AnalysisTruncation, AnalysisTruncationKind, AssetAnalysis, AssetAnalysisBatch,
    ReferenceDependencyKey, ReferenceResolutionProjection,
};
use crate::analyzer::{AnalysisError, AnalyzerLimits, AssetAnalyzer, WorkspaceAnalysisContext};
use crate::config::{IndexPaths, SearchIndexOptions};
use crate::contract::{ApiErrorCode, ReferencesResponse, SearchResponse, SuggestResponse};
use crate::generation::{
    GenerationManifestError, GenerationProjectionDigests, GenerationProjectionSummary,
    GenerationStamp, ReindexAnalysisEvidence, ReindexDisposition, ReindexEvidence, ReindexIntent,
    ReindexReceipt, ReindexScope, SEARCH_GENERATION_CONTRACT_VERSION, SearchGenerationIdentityV1,
    SearchGenerationManifestV1,
};
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
    FileHint, PathRejection, ProjectScanner, ReadSource, ScanDiagnostic, ScanError, ScanIntent,
    ScanMetrics, ScanMode, SourceHints, SourcePart,
};
#[cfg(test)]
use crate::state::GenerationFailpoint;
use crate::state::{
    GenerationDiskEstimate, GenerationSnapshot, GenerationStore, GenerationStoreError,
    GenerationStoreOptions, SourceScanHint, SourceStateError, SourceStateLimits,
    SourceStateSnapshot, TransactionReceiptMembership, TransactionReceiptWindow,
};
use crate::store::{ProjectionReaders, ProjectionStore};

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
    pub(crate) dependency_closure_assets: u64,
    pub(crate) workspace_parse_failures: u64,
    pub(crate) scan_diagnostics: u64,
    pub(crate) forced_full_scan: bool,
    pub(crate) forced_full_analysis: bool,
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
        projection_options_match: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PipelineError> {
        let manifest = snapshot.manifest();
        let stamp = GenerationStamp::current(
            snapshot.generation(),
            manifest.workspace(),
            manifest.revision(),
        );
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
            None if projection_options_match => QuerySnapshot::new(
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
            || projection_options_match && projection_is_complete(source_state, options),
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
        response.generation = self.stamp.clone();
        Ok(response)
    }

    pub(crate) fn suggest(&self, prefix: &str, limit: usize) -> SuggestResponse {
        let mut response = self.query.suggest(prefix, limit);
        response.generation = self.stamp.clone();
        response
    }

    pub(crate) fn references(
        &self,
        request: crate::contract::ReferenceRequest,
    ) -> Result<ReferencesResponse, ReferenceQueryError> {
        let mut response = self.references.references(request)?;
        response.generation = self.stamp.clone();
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

impl PipelineBuildOutput {
    pub(crate) fn receipt(&self) -> ReindexReceipt {
        ReindexReceipt {
            contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
            disposition: match self.disposition {
                PipelineBuildDisposition::AlreadyApplied => ReindexDisposition::AlreadyApplied,
                PipelineBuildDisposition::Published | PipelineBuildDisposition::NoChange => {
                    ReindexDisposition::Applied
                }
            },
            transaction: self.transaction,
            target_revision: self.target_revision,
            generation: self.active.as_ref().map(|active| active.stamp.clone()),
            evidence: ReindexEvidence {
                forced_full_scan: self.metrics.forced_full_scan,
                forced_full_analysis: self.metrics.forced_full_analysis,
                dependency_closure_assets: self.metrics.dependency_closure_assets,
                analysis: ReindexAnalysisEvidence::from(&self.metrics.analysis),
                disk_estimate: self.disk_estimate.map(Into::into),
                publish_warnings: self.warnings.clone(),
            },
        }
    }
}

impl From<&AnalysisMetrics> for ReindexAnalysisEvidence {
    fn from(metrics: &AnalysisMetrics) -> Self {
        Self {
            assets_visited: metrics.assets_visited,
            assets_analyzed: metrics.assets_analyzed,
            source_opens: metrics.source_opens,
            source_bytes_read: metrics.source_bytes_read,
            text_sources: metrics.text_sources,
            text_bytes_scanned: metrics.text_bytes_scanned,
            yaml_documents: metrics.yaml_documents,
            binary_objects: metrics.binary_objects,
            unity_values_visited: metrics.unity_values_visited,
            references_emitted: metrics.references_emitted,
            container_entries_emitted: metrics.container_entries_emitted,
            truncations_emitted: metrics.truncations_emitted,
            diagnostics_emitted: metrics.diagnostics_emitted,
        }
    }
}

pub(crate) struct SearchGenerationPipeline {
    paths: IndexPaths,
    options: SearchIndexOptions,
    options_digest: DigestV1,
    scanner: ProjectScanner,
    workspace: AssetWorkspace,
    workspace_roots: WorkspaceRoots,
    workspace_hydrated: bool,
    active_options_match: bool,
    store: GenerationStore,
    source_state: Option<SourceStateSnapshot>,
    active: Option<Arc<ActiveGeneration>>,
    #[cfg(test)]
    publish_failpoint: Option<GenerationFailpoint>,
}

impl fmt::Debug for SearchGenerationPipeline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchGenerationPipeline")
            .field("paths", &self.paths)
            .field("options", &self.options)
            .field("workspace", &self.workspace)
            .field("workspace_hydrated", &self.workspace_hydrated)
            .field("active_options_match", &self.active_options_match)
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
        let options = options.validate().map_err(PipelineError::Configuration)?;
        let options_digest = options
            .logical_digest()
            .map_err(PipelineError::Configuration)?;
        let scanner = ProjectScanner::new(&paths, options, options.scan_limits())
            .map_err(PipelineError::Configuration)?;
        let store = GenerationStore::open(
            paths.index_root(),
            GenerationStoreOptions {
                retain_previous_generations: options.retain_previous_generations,
            },
        )?;
        let recovered = store.active().cloned();
        let source_state = recovered
            .as_ref()
            .map(|snapshot| snapshot.load_source_state(budget, SourceStateLimits::default()))
            .transpose()?;
        let workspace = match source_state.as_ref() {
            Some(state) => {
                AssetWorkspace::with_workspace_id(state.workspace(), WorkspaceOptions::lenient())?
            }
            None => AssetWorkspace::new()?,
        };
        let active_options_match = recovered
            .as_ref()
            .is_none_or(|snapshot| snapshot.manifest().options_digest() == options_digest);
        let active = match (recovered, source_state.as_ref()) {
            (Some(snapshot), Some(state)) => {
                let readers = ProjectionReaders::open(snapshot.directory())
                    .map_err(PipelineError::Projection)?;
                Some(Arc::new(ActiveGeneration::open(
                    snapshot,
                    state,
                    &readers,
                    None,
                    options,
                    active_options_match,
                    budget,
                )?))
            }
            (None, None) => None,
            _ => {
                return Err(PipelineError::Invariant(
                    "generation recovery produced only one half of the active state",
                ));
            }
        };
        Ok(Self {
            paths,
            options,
            options_digest,
            scanner,
            workspace,
            workspace_roots: WorkspaceRoots::default(),
            workspace_hydrated: false,
            active_options_match,
            store,
            source_state,
            active,
            #[cfg(test)]
            publish_failpoint: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn inject_publish_failpoint(&mut self, failpoint: GenerationFailpoint) {
        self.publish_failpoint = Some(failpoint);
    }

    pub(crate) fn active(&self) -> Option<Arc<ActiveGeneration>> {
        self.active.clone()
    }

    pub(crate) fn reindex_filesystem(
        &mut self,
        intent: ReindexIntent,
        budget: &mut AssetLoadBudget,
    ) -> Result<PipelineBuildOutput, PipelineError> {
        validate_intent_version(&intent)?;
        let started = Instant::now();
        let requested = match intent.scope {
            ReindexScope::Full => ScanIntent::Full,
            ReindexScope::Reconcile => ScanIntent::Reconcile,
            ReindexScope::ChangedPaths { paths } => ScanIntent::ChangedPaths(paths),
            ReindexScope::ChangeSet { .. } => {
                return Err(PipelineError::WorkspaceViewRequired);
            }
        };
        let force_full = !self.workspace_hydrated || !self.active_options_match;
        let scan_intent = if force_full {
            ScanIntent::Full
        } else {
            requested
        };
        let prepared = self.prepare_filesystem_batch(scan_intent, force_full, budget)?;
        self.publish_batch(
            prepared.batch,
            prepared.scan_hints,
            prepared.metrics,
            prepared.transaction_receipts,
            prepared.workspace,
            None,
            None,
            started,
            budget,
        )
    }

    pub(crate) fn reindex_workspace(
        &mut self,
        changes: ChangeSet,
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<PipelineBuildOutput, PipelineError> {
        let started = Instant::now();
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
            match self
                .source_state
                .as_ref()
                .ok_or(PipelineError::Invariant(
                    "indexed workspace disappeared during receipt lookup",
                ))?
                .transaction_membership(&changes, budget)?
            {
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
                TransactionReceiptMembership::Conflict { .. } => {
                    return Err(PipelineError::TransactionConflict {
                        transaction: changes.transaction(),
                    });
                }
                TransactionReceiptMembership::Absent { .. } => {}
            }
            self.mark_desired_revision(changes.to_revision());
            validate_change_set_view(&changes, view)?;
            if revision != changes.from_revision() {
                return Err(PipelineError::RevisionBarrierMismatch {
                    indexed: revision,
                    change_from: changes.from_revision(),
                    change_to: changes.to_revision(),
                });
            }
        } else {
            validate_change_set_view(&changes, view)?;
        }

        let prepared = self.prepare_workspace_batch(&changes, view, budget)?;
        self.publish_batch(
            prepared.batch,
            prepared.scan_hints,
            prepared.metrics,
            prepared.transaction_receipts,
            prepared.workspace,
            Some(changes.transaction()),
            Some(changes.to_revision()),
            started,
            budget,
        )
    }

    fn prepare_filesystem_batch(
        &mut self,
        intent: ScanIntent,
        forced_full_scan: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedBatch, PipelineError> {
        let mut cached = self.clone_previous_assets(budget)?;
        let mut workspace = self.workspace.clone();
        let mut workspace_roots = self.workspace_roots.try_clone_with_budget(budget)?;
        let known_paths = clone_known_paths(&cached, budget)?;
        let plan =
            self.scanner
                .plan(intent, &known_paths, budget)
                .map_err(|error| match error {
                    ScanError::Budget(error) => PipelineError::Budget(error),
                    error => PipelineError::Scan(error.into()),
                })?;
        let invalid_changed_path = matches!(plan.mode, ScanMode::ChangedPaths)
            && plan.diagnostics.iter().any(|diagnostic| {
                matches!(
                    diagnostic,
                    ScanDiagnostic::PathRejected {
                        reason: PathRejection::InvalidPath
                            | PathRejection::OutsideProject
                            | PathRejection::Symlink
                            | PathRejection::UnsupportedFileType
                            | PathRejection::NonUtf8RelativePath,
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
                ScanDiagnostic::WalkFailed { .. } | ScanDiagnostic::ReadFailed { .. }
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
        for candidate in &plan.present {
            let previous_identity = cached
                .get(&candidate.rel_path)
                .map(|analysis| analysis.source.content_digest);
            let outcome = self
                .scanner
                .read_source(candidate, previous_identity, budget);
            metrics.scan.merge(&outcome.metrics);
            metrics.scan_diagnostics = metrics
                .scan_diagnostics
                .saturating_add(saturating_usize_to_u64(outcome.diagnostics.len()));
            let source = match outcome.source {
                Some(source) => source,
                None => {
                    return Err(PipelineError::SourceReadRejected {
                        relative_path: candidate.rel_path.clone(),
                        diagnostics: outcome.diagnostics,
                    });
                }
            };
            read_sources.push(ScannedSource {
                source,
                diagnostics: outcome.diagnostics,
            });
        }
        read_sources
            .sort_unstable_by(|left, right| left.source.rel_path.cmp(&right.source.rel_path));
        if read_sources
            .windows(2)
            .any(|pair| pair[0].source.rel_path == pair[1].source.rel_path)
        {
            return Err(PipelineError::Invariant(
                "scanner returned duplicate relative paths",
            ));
        }

        let changed_read_sources = read_sources
            .iter()
            .filter(|scanned| {
                !scanned.source.unchanged || !cached.contains_key(scanned.source.rel_path.as_str())
            })
            .count();
        let changed_path_capacity = plan.deleted.len().checked_add(changed_read_sources).ok_or(
            PipelineError::ArithmeticOverflow("changed source path list"),
        )?;
        let mut changed_paths =
            reserve_retained_vec(changed_path_capacity, "changed source path list", budget)?;
        changed_paths.extend(plan.deleted.iter().map(String::as_str));
        for scanned in &read_sources {
            let source = &scanned.source;
            if !source.unchanged || !cached.contains_key(&source.rel_path) {
                changed_paths.push(source.rel_path.as_str());
            }
        }
        changed_paths.sort_unstable();
        changed_paths.dedup();
        let mut impact = DependencyImpact::default();
        for relative_path in &changed_paths {
            if let Some(analysis) = cached.get(relative_path) {
                impact.add_analysis_identity(analysis, budget)?;
            }
        }
        for scanned in &read_sources {
            let source = &scanned.source;
            if changed_paths
                .binary_search(&source.rel_path.as_str())
                .is_ok()
            {
                impact.add_guid(source.guid.as_deref(), budget)?;
                impact.add_source_path(source.rel_path.as_str(), budget)?;
            }
        }
        let changed_evidence_incomplete = changed_paths.iter().any(|relative_path| {
            cached.get(relative_path).is_some_and(|analysis| {
                analysis.source.workspace_source.is_some() && !analysis.graph_inputs.complete
            })
        });

        let mut scan_hints = self.previous_scan_hints(budget)?;
        for deleted in &plan.deleted {
            remove_scan_hint(&mut scan_hints, deleted);
            if let Some(root) = workspace_roots.remove(deleted) {
                workspace.unload_source(root, budget)?;
            }
        }

        let was_hydrated = self.workspace_hydrated;
        let mut parse_failures =
            reserve_retained_vec(read_sources.len(), "workspace parse failures", budget)?;
        parse_failures.resize_with(read_sources.len(), || None::<String>);
        for (source_index, scanned) in read_sources.iter().enumerate() {
            let source = &scanned.source;
            upsert_scan_hint(&mut scan_hints, source, budget)?;
            let root_is_loaded = workspace_roots.contains_key(&source.rel_path);
            let needs_reload = !was_hydrated || !source.unchanged || !root_is_loaded;
            if !needs_reload {
                continue;
            }
            if let Some(root) = workspace_roots.get(&source.rel_path).copied() {
                workspace.unload_source(root, budget)?;
            }
            if !is_workspace_candidate(source, budget)? {
                workspace_roots.remove(&source.rel_path);
                continue;
            }
            let Some(bytes) = source.bytes.as_ref() else {
                workspace_roots.remove(&source.rel_path);
                continue;
            };
            let inserts_workspace_root =
                workspace_roots.prepare_upsert(&source.rel_path, budget)?;
            charge_retained_string(&source.rel_path, "workspace source alias", budget)?;
            charge_retained_path(&source.abs_path, "workspace source path", budget)?;
            let alias = SourceAlias::new(clone_precharged_string(
                &source.rel_path,
                "workspace source alias",
            )?)?;
            let request = SourceOpenRequest::new(
                clone_precharged_path(&source.abs_path, "workspace source path")?,
                alias,
            );
            match workspace.load_budgeted_source_bytes(request, bytes.clone(), budget) {
                Ok(root) => {
                    workspace_roots.insert_precharged(
                        &source.rel_path,
                        root,
                        inserts_workspace_root,
                    )?;
                }
                Err(error) if workspace_parse_can_fall_back(&error) => {
                    workspace_roots.remove(&source.rel_path);
                    metrics.workspace_parse_failures =
                        metrics.workspace_parse_failures.saturating_add(1);
                    parse_failures[source_index] = Some(retained_display_string(
                        &error,
                        "workspace parse failure",
                        budget,
                    )?);
                }
                Err(error) => return Err(error.into()),
            }
        }
        let workspace_hydrated =
            was_hydrated || matches!(plan.mode, ScanMode::Full | ScanMode::Reconcile);
        self.mark_desired_revision(workspace.revision());

        let snapshot = workspace.snapshot();
        let graph = snapshot.reference_graph(ReferenceGraphBuildOptions::unbounded(), budget)?;
        let context = WorkspaceAnalysisContext::build(&snapshot, &graph, budget)?;
        let mut current_sources = snapshot.sources(budget)?;
        current_sources.sort_unstable_by_key(|source| source.id());
        let changed_root_count = changed_paths
            .iter()
            .filter_map(|relative_path| workspace_roots.get(relative_path).copied())
            .count();
        let mut changed_roots =
            reserve_retained_vec(changed_root_count, "changed workspace roots", budget)?;
        changed_roots.extend(
            changed_paths
                .iter()
                .filter_map(|relative_path| workspace_roots.get(relative_path).copied()),
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
        let force_full_analysis =
            !changed_paths.is_empty() && (!graph.is_complete() || changed_evidence_incomplete);
        metrics.forced_full_analysis = force_full_analysis;
        let affected_path_count = cached
            .iter()
            .filter(|(relative_path, analysis)| {
                if changed_paths.binary_search(relative_path).is_ok()
                    || analysis.source.workspace_source.is_none()
                {
                    return false;
                }
                force_full_analysis || !analysis.complete || impact.matches_analysis(analysis)
            })
            .count();
        for (relative_path, analysis) in cached.iter() {
            if changed_paths.binary_search(&relative_path).is_err()
                && analysis.source.workspace_source.is_some()
                && (force_full_analysis || !analysis.complete || impact.matches_analysis(analysis))
            {
                charge_retained_string(relative_path, "affected source path", budget)?;
            }
        }
        let mut affected_paths =
            reserve_retained_vec(affected_path_count, "affected source paths", budget)?;
        for (relative_path, analysis) in cached.iter() {
            if changed_paths.binary_search(&relative_path).is_err()
                && analysis.source.workspace_source.is_some()
                && (force_full_analysis || !analysis.complete || impact.matches_analysis(analysis))
            {
                affected_paths.push(clone_precharged_string(
                    relative_path,
                    "affected source path",
                )?);
            }
        }
        drop(impact);
        for deleted in &plan.deleted {
            cached.remove(deleted);
        }
        metrics.dependency_closure_assets = saturating_usize_to_u64(affected_paths.len());
        let analyzer = AssetAnalyzer::new(AnalyzerLimits::default());
        let replaced_asset_count = read_sources
            .iter()
            .filter(|scanned| cached.contains_key(&scanned.source.rel_path))
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
            let relative_path = source.rel_path.as_str();
            let cached_analysis = cached.take(relative_path);
            let workspace_root = workspace_roots.get(relative_path).copied();
            let workspace_input = workspace_root.map(|root| context.asset(root)).transpose()?;
            let analyzed = match cached_analysis {
                Some(mut cached_analysis) if source.unchanged => {
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
                        && affected_paths
                            .binary_search_by(|path| path.as_str().cmp(relative_path))
                            .is_err()
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

        for (relative_path, cached_analysis) in cached.into_remaining() {
            if cached_analysis.source.workspace_source.is_none()
                || affected_paths
                    .binary_search_by(|path| path.as_str().cmp(&relative_path))
                    .is_err()
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

        let transaction_receipts = match &self.source_state {
            Some(state) if state.workspace() == snapshot.workspace_id() => {
                state.transaction_receipts().try_clone_with_budget(budget)?
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
        })
    }

    fn prepare_workspace_batch(
        &self,
        changes: &ChangeSet,
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedBatch, PipelineError> {
        let transaction_receipts = match &self.source_state {
            Some(state) => state.transaction_receipts_after(changes, budget)?,
            None => TransactionReceiptWindow::from_change_set(changes, budget)?,
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
                    || changes.changed_sources().contains(&root)
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
                        && object.yaml_anchor().is_none()
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
            dependency_closure_assets: saturating_usize_to_u64(affected_roots.len()),
            forced_full_analysis: force_full_analysis,
            ..PipelineBuildMetrics::default()
        };
        let mut scan_hints = self.previous_scan_hints(budget)?;
        let roots = workspace_root_paths(&self.paths, &sources, budget)?;
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

        for (relative_path, root_id) in roots {
            let root = workspace_source(&sources, root_id)
                .ok_or(PipelineError::UnknownWorkspaceSource(root_id))?;
            let length = view.source_length(root.id())?;
            let cached_analysis = cached
                .take(&relative_path)
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
            upsert_scan_hint(&mut scan_hints, &source, budget)?;
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
        scan_hints.retain(|hint| {
            assets
                .iter()
                .any(|analysis| analysis.source.relative_path == hint.relative_path)
        });

        let transactions = canonical_transaction_ids(&transaction_receipts, budget)?;
        let batch = AssetAnalysisBatch::new(
            view.workspace_id(),
            view.revision(),
            transactions,
            assets,
            metrics.analysis,
        );
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
        })
    }

    fn publish_batch(
        &mut self,
        batch: AssetAnalysisBatch,
        scan_hints: Vec<SourceScanHint>,
        mut metrics: PipelineBuildMetrics,
        transaction_receipts: TransactionReceiptWindow,
        workspace: Option<PreparedWorkspace>,
        transaction: Option<TransactionId>,
        target_revision: Option<WorkspaceRevision>,
        started: Instant,
        budget: &mut AssetLoadBudget,
    ) -> Result<PipelineBuildOutput, PipelineError> {
        if self.active_options_match
            && self
                .source_state
                .as_ref()
                .is_some_and(|state| batch_matches_state(&batch, state))
        {
            self.active_options_match = true;
            self.install_workspace(workspace);
            return Ok(PipelineBuildOutput {
                disposition: PipelineBuildDisposition::NoChange,
                active: self.active.clone(),
                metrics,
                disk_estimate: None,
                warnings: Vec::new(),
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
            SourceStateSnapshot::from_batch(batch, scan_hints, transaction_receipts)?;
        let build = self.store.begin()?;
        let projection_evidence = ProjectionStore::build(build.directory(), &projection)
            .map_err(PipelineError::Projection)?;
        build.write_source_state(&source_state, SourceStateLimits::default())?;

        {
            let readers =
                ProjectionReaders::open(build.directory()).map_err(PipelineError::Projection)?;
            SearchQueryFields::from_schema(&readers.search().index().schema())
                .map_err(PipelineError::Query)?;
        }

        let artifacts = self.store.measure_artifacts(&build)?;
        let expected_artifacts = projection_evidence.generation_artifacts(artifacts.source_state());
        if artifacts != expected_artifacts {
            return Err(PipelineError::Invariant(
                "projection evidence changed before generation publication",
            ));
        }
        let parent = self.store.active().map(GenerationSnapshot::generation);
        let projection_summary = GenerationProjectionSummary::new(
            u64::try_from(source_state.assets().len())
                .map_err(|_| PipelineError::ArithmeticOverflow("indexed asset count"))?,
            projection.metrics.search_documents,
            projection.metrics.reference_documents,
            u64::try_from(projection.truncations.len())
                .map_err(|_| PipelineError::ArithmeticOverflow("projection truncation count"))?,
            u64::try_from(
                source_state
                    .assets()
                    .iter()
                    .filter(|analysis| !analysis.complete)
                    .count(),
            )
            .map_err(|_| PipelineError::ArithmeticOverflow("incomplete asset count"))?,
        )?;
        let identity = SearchGenerationIdentityV1::new(
            source_state.workspace(),
            source_state.revision(),
            GenerationProjectionDigests::new(
                projection_evidence.logical_digests().search(),
                projection_evidence.logical_digests().references(),
            ),
            projection_summary,
            parent,
            canonical_transaction_ids(source_state.transaction_receipts(), budget)?,
            self.options_digest,
            source_state.logical_digest(),
        )?;
        let manifest = SearchGenerationManifestV1::new(identity, artifacts);
        let disk_estimate = self.store.estimate_manifest_publish(&manifest)?;
        #[cfg(test)]
        let prepared = match self.publish_failpoint.take() {
            Some(failpoint) => self
                .store
                .prepare_publish_with_failpoint(build, manifest, failpoint)?,
            None => self.store.prepare_publish(build, manifest)?,
        };
        #[cfg(not(test))]
        let prepared = self.store.prepare_publish(build, manifest)?;
        let candidate_snapshot = prepared.snapshot().clone();
        let readers = ProjectionReaders::open(candidate_snapshot.directory())
            .map_err(PipelineError::Projection)?;
        let active = Arc::new(ActiveGeneration::open(
            candidate_snapshot,
            &source_state,
            &readers,
            Some(&projection),
            self.options,
            true,
            budget,
        )?);
        let report = prepared.activate()?;
        let warnings = report
            .warnings
            .into_iter()
            .map(|warning| warning.to_string())
            .collect();
        self.source_state = Some(source_state);
        self.active = Some(Arc::clone(&active));
        self.active_options_match = true;
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
            });
        };
        for analysis in state.assets() {
            charge_retained_string(&analysis.source.relative_path, "cached asset path", budget)?;
            charge_cached_analysis_clone(analysis, budget)?;
        }
        let mut entries = reserve_retained_vec(state.assets().len(), "cached asset index", budget)?;
        for analysis in state.assets() {
            let relative_path =
                clone_precharged_string(&analysis.source.relative_path, "cached asset path")?;
            entries.push(CachedAsset {
                relative_path,
                analysis: Some(analysis.clone()),
            });
        }
        entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(CachedAssets { entries })
    }

    fn previous_scan_hints(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceScanHint>, PipelineError> {
        let Some(state) = &self.source_state else {
            return Ok(Vec::new());
        };
        for hint in state.scan_hints() {
            charge_retained_string(&hint.relative_path, "source scan hint path clone", budget)?;
        }
        let mut hints =
            reserve_retained_vec(state.scan_hints().len(), "source scan hints", budget)?;
        for hint in state.scan_hints() {
            hints.push(SourceScanHint::new(
                clone_precharged_string(&hint.relative_path, "source scan hint path clone")?,
                hint.source_length,
                hint.source_modified_unix_ms,
                hint.metadata_length,
                hint.metadata_modified_unix_ms,
            )?);
        }
        Ok(hints)
    }

    fn mark_desired_revision(&mut self, desired: WorkspaceRevision) {
        if let Some(active) = &mut self.active {
            Arc::make_mut(active).set_desired_revision(desired);
        }
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

struct PreparedBatch {
    batch: AssetAnalysisBatch,
    scan_hints: Vec<SourceScanHint>,
    metrics: PipelineBuildMetrics,
    transaction_receipts: TransactionReceiptWindow,
    workspace: Option<PreparedWorkspace>,
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

#[derive(Default)]
struct WorkspaceRoots {
    entries: Vec<(String, SourceId)>,
}

impl WorkspaceRoots {
    fn get(&self, relative_path: &str) -> Option<&SourceId> {
        let index = self
            .entries
            .binary_search_by(|(path, _)| path.as_str().cmp(relative_path))
            .ok()?;
        Some(&self.entries[index].1)
    }

    fn contains_key(&self, relative_path: &str) -> bool {
        self.get(relative_path).is_some()
    }

    fn remove(&mut self, relative_path: &str) -> Option<SourceId> {
        let index = self
            .entries
            .binary_search_by(|(path, _)| path.as_str().cmp(relative_path))
            .ok()?;
        Some(self.entries.remove(index).1)
    }

    fn try_clone_with_budget(&self, budget: &mut AssetLoadBudget) -> Result<Self, PipelineError> {
        let mut entries =
            reserve_retained_vec(self.entries.len(), "workspace root index clone", budget)?;
        for (path, root) in &self.entries {
            charge_retained_string(path, "workspace root path clone", budget)?;
            entries.push((
                clone_precharged_string(path, "workspace root path clone")?,
                *root,
            ));
        }
        Ok(Self { entries })
    }

    fn prepare_upsert(
        &mut self,
        relative_path: &str,
        budget: &mut AssetLoadBudget,
    ) -> Result<bool, PipelineError> {
        if self.contains_key(relative_path) {
            return Ok(false);
        }
        prepare_retained_vec_push(&mut self.entries, "workspace root index entry", budget)?;
        charge_retained_string(relative_path, "workspace root path", budget)?;
        Ok(true)
    }

    fn insert_precharged(
        &mut self,
        relative_path: &str,
        root: SourceId,
        inserts: bool,
    ) -> Result<(), PipelineError> {
        match self
            .entries
            .binary_search_by(|(path, _)| path.as_str().cmp(relative_path))
        {
            Ok(index) if !inserts => {
                self.entries[index].1 = root;
                Ok(())
            }
            Ok(_) => Err(PipelineError::Invariant(
                "workspace root insertion replaced an existing path",
            )),
            Err(index) if inserts => {
                let relative_path = clone_precharged_string(relative_path, "workspace root path")?;
                self.entries.insert(index, (relative_path, root));
                Ok(())
            }
            Err(_) => Err(PipelineError::Invariant(
                "precharged workspace root path disappeared before update",
            )),
        }
    }
}

struct CachedAsset {
    relative_path: String,
    analysis: Option<AssetAnalysis>,
}

struct CachedAssets {
    entries: Vec<CachedAsset>,
}

impl CachedAssets {
    fn get(&self, relative_path: &str) -> Option<&AssetAnalysis> {
        let index = self
            .entries
            .binary_search_by(|entry| entry.relative_path.as_str().cmp(relative_path))
            .ok()?;
        self.entries[index].analysis.as_ref()
    }

    fn contains_key(&self, relative_path: &str) -> bool {
        self.get(relative_path).is_some()
    }

    fn take(&mut self, relative_path: &str) -> Option<AssetAnalysis> {
        let index = self
            .entries
            .binary_search_by(|entry| entry.relative_path.as_str().cmp(relative_path))
            .ok()?;
        self.entries[index].analysis.take()
    }

    fn remove(&mut self, relative_path: &str) {
        let _ = self.take(relative_path);
    }

    fn take_workspace_root(&mut self, root: SourceId) -> Option<AssetAnalysis> {
        let entry = self.entries.iter_mut().find(|entry| {
            entry
                .analysis
                .as_ref()
                .is_some_and(|analysis| analysis.source.workspace_source == Some(root))
        })?;
        entry.analysis.take()
    }

    fn values(&self) -> impl Iterator<Item = &AssetAnalysis> {
        self.entries
            .iter()
            .filter_map(|entry| entry.analysis.as_ref())
    }

    fn iter(&self) -> impl Iterator<Item = (&str, &AssetAnalysis)> {
        self.entries.iter().filter_map(|entry| {
            entry
                .analysis
                .as_ref()
                .map(|analysis| (entry.relative_path.as_str(), analysis))
        })
    }

    fn retain(&mut self, mut predicate: impl FnMut(&str, &AssetAnalysis) -> bool) {
        for entry in &mut self.entries {
            let retain = entry
                .analysis
                .as_ref()
                .is_none_or(|analysis| predicate(&entry.relative_path, analysis));
            if !retain {
                let _ = entry.analysis.take();
            }
        }
    }

    fn len(&self) -> usize {
        self.values().count()
    }

    fn into_remaining(self) -> impl Iterator<Item = (String, AssetAnalysis)> {
        self.entries.into_iter().filter_map(|entry| {
            entry
                .analysis
                .map(|analysis| (entry.relative_path, analysis))
        })
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
        && state
            .transaction_receipts()
            .matches_canonical_ids(&batch.transactions)
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

fn validate_intent_version(intent: &ReindexIntent) -> Result<(), PipelineError> {
    if intent.contract_version == SEARCH_GENERATION_CONTRACT_VERSION {
        Ok(())
    } else {
        Err(PipelineError::UnsupportedContractVersion {
            actual: intent.contract_version,
            expected: SEARCH_GENERATION_CONTRACT_VERSION,
        })
    }
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
                    && address.yaml_anchor() == candidate.object.yaml_anchor()
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

fn charge_retained_path(
    value: &Path,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    let requested = value.as_os_str().len();
    let bytes =
        u64::try_from(requested).map_err(|_| PipelineError::ArithmeticOverflow(resource))?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(())
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
    cached: &CachedAssets,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<String>, PipelineError> {
    let mut known_paths = reserve_retained_vec(cached.len(), "known source path index", budget)?;
    for (path, _) in cached.iter() {
        charge_retained_string(path, "known source path", budget)?;
        let owned = clone_precharged_string(path, "known source path")?;
        known_paths.push(owned);
    }
    if !known_paths
        .windows(2)
        .all(|pair| pair[0].as_str() < pair[1].as_str())
    {
        return Err(PipelineError::Invariant(
            "cached source paths were not uniquely sorted",
        ));
    }
    Ok(known_paths)
}

fn workspace_root_paths(
    paths: &IndexPaths,
    sources: &[WorkspaceSource],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<(String, SourceId)>, PipelineError> {
    let root_count = sources
        .iter()
        .filter(|source| source.parent().is_none())
        .count();
    for root in sources.iter().filter(|source| source.parent().is_none()) {
        let retained_bytes = match root.physical_origin() {
            Some(origin) => match origin.strip_prefix(paths.project_root()) {
                Ok(relative) if !relative.as_os_str().is_empty() => {
                    portable_relative_path_len(relative)?
                }
                Ok(_) | Err(_) => root.locator().root_alias().as_str().len(),
            },
            None => root.locator().root_alias().as_str().len(),
        };
        let retained_bytes = u64::try_from(retained_bytes)
            .map_err(|_| PipelineError::ArithmeticOverflow("workspace root path"))?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_bytes(retained_bytes)?;
    }
    let mut by_path = reserve_retained_vec(root_count, "workspace root paths", budget)?;
    for root in sources.iter().filter(|source| source.parent().is_none()) {
        let relative_path = match root.physical_origin() {
            Some(origin) => match origin.strip_prefix(paths.project_root()) {
                Ok(relative) if !relative.as_os_str().is_empty() => {
                    portable_relative_path_precharged(relative)?
                }
                Ok(_) | Err(_) => clone_precharged_string(
                    root.locator().root_alias().as_str(),
                    "workspace root path",
                )?,
            },
            None => clone_precharged_string(
                root.locator().root_alias().as_str(),
                "workspace root path",
            )?,
        };
        by_path.push((relative_path, root.id()));
    }
    by_path.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for index in 1..by_path.len() {
        if by_path[index - 1].0 == by_path[index].0 {
            let (relative_path, second) = by_path.remove(index);
            let first = by_path[index - 1].1;
            return Err(PipelineError::RelativePathCollision {
                relative_path,
                first,
                second,
            });
        }
    }
    Ok(by_path)
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
    SourceScanHint::new(
        clone_precharged_string(&source.rel_path, "source scan hint path")?,
        source.hints.asset.size,
        source.hints.asset.mtime_ms,
        source.hints.meta.map(|hint| hint.size),
        source.hints.meta.and_then(|hint| hint.mtime_ms),
    )
    .map_err(PipelineError::from)
}

fn upsert_scan_hint(
    hints: &mut Vec<SourceScanHint>,
    source: &ReadSource,
    budget: &mut AssetLoadBudget,
) -> Result<(), PipelineError> {
    let position = hints.binary_search_by(|hint| hint.relative_path.as_str().cmp(&source.rel_path));
    charge_retained_string(&source.rel_path, "source scan hint path", budget)?;
    if position.is_err() {
        prepare_retained_vec_push(hints, "source scan hints", budget)?;
    }
    let hint = source_scan_hint_precharged(source)?;
    match position {
        Ok(index) => hints[index] = hint,
        Err(index) => hints.insert(index, hint),
    }
    Ok(())
}

fn remove_scan_hint(hints: &mut Vec<SourceScanHint>, relative_path: &str) {
    if let Ok(index) = hints.binary_search_by(|hint| hint.relative_path.as_str().cmp(relative_path))
    {
        hints.remove(index);
    }
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
                analysis.diagnostics.sort();
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
    if matches!(
        source.kind,
        SearchKind::Prefab
            | SearchKind::Scene
            | SearchKind::Material
            | SearchKind::AnimationClip
            | SearchKind::AnimatorController
            | SearchKind::Asset
    ) {
        return Ok(true);
    }
    let Some(bytes) = source.bytes.as_ref() else {
        return Ok(false);
    };
    bytes.validate_budget(budget)?;
    let bytes = bytes.as_bytes();
    if bytes.starts_with(b"%YAML")
        || bytes.starts_with(b"--- !u!")
        || bytes.starts_with(b"UnityFS")
        || bytes.starts_with(b"UnityWeb")
        || bytes.starts_with(b"UnityRaw")
        || bytes.starts_with(b"PK\x03\x04")
    {
        return Ok(true);
    }
    Ok(Path::new(&source.rel_path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["assets", "sharedassets", "bundle", "unity3d", "zip", "apk"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        }))
}

fn workspace_parse_can_fall_back(error: &WorkspaceError) -> bool {
    match error {
        WorkspaceError::InvalidSource { .. }
        | WorkspaceError::UnsupportedSource { .. }
        | WorkspaceError::InvalidSourceIdentity { .. }
        | WorkspaceError::BinaryMember { .. }
        | WorkspaceError::InvalidSourceMemberIdentity { .. } => true,
        WorkspaceError::Operation { operation, .. } => matches!(
            *operation,
            "YAML source parsing" | "ZIP archive preflight" | "ZIP archive loading"
        ),
        WorkspaceError::Binary(error) => !matches!(
            error,
            BinaryError::Budget(_)
                | BinaryError::MemoryError(_)
                | BinaryError::ResourceLimitExceeded(_)
        ),
        WorkspaceError::Budget(_)
        | WorkspaceError::Contract(_)
        | WorkspaceError::Io { .. }
        | WorkspaceError::SourceChanged { .. }
        | WorkspaceError::SourceTooLarge { .. }
        | WorkspaceError::Allocation { .. }
        | WorkspaceError::NotRootSource(_)
        | WorkspaceError::MissingSource(_)
        | WorkspaceError::MissingObject(_)
        | WorkspaceError::AmbiguousObject { .. }
        | WorkspaceError::RangeOverflow { .. }
        | WorkspaceError::RangeOutOfBounds { .. }
        | WorkspaceError::PreparedArtifact(_)
        | WorkspaceError::PreparedSourceKindMismatch { .. }
        | WorkspaceError::PreparedArtifactKindMismatch { .. }
        | WorkspaceError::PreparedArtifactDigestMismatch { .. }
        | WorkspaceError::PreparedArtifactLengthMismatch { .. }
        | WorkspaceError::PreparedArtifactSourceProvenanceMismatch { .. }
        | WorkspaceError::PreparedArtifactFingerprintProvenanceMismatch { .. } => false,
    }
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

#[derive(Debug)]
pub(crate) enum PipelineError {
    UnsupportedContractVersion {
        actual: u16,
        expected: u16,
    },
    WorkspaceViewRequired,
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
    Scan(anyhow::Error),
    Query(anyhow::Error),
    Projection(anyhow::Error),
    Budget(BudgetError),
    Contract(ContractError),
    Diagnostic(DiagnosticError),
    Workspace(Box<WorkspaceError>),
    ReferenceGraph(Box<ReferenceGraphError>),
    Analysis(Box<AnalysisError>),
    SourceState(Box<SourceStateError>),
    Store(Box<GenerationStoreError>),
    Manifest(GenerationManifestError),
    Json(serde_json::Error),
}

impl PipelineError {
    pub(crate) fn api_code(&self) -> ApiErrorCode {
        match self {
            Self::UnsupportedContractVersion { .. }
            | Self::WorkspaceViewRequired
            | Self::Configuration(_)
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
            Self::SourceState(error)
                if matches!(
                    error.as_ref(),
                    SourceStateError::Store(store_error)
                        if matches!(store_error.as_ref(), GenerationStoreError::WriterLeaseUnavailable { .. })
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
            Self::ScanPlanRejected { .. } | Self::SourceReadRejected { .. } | Self::Scan(_) => true,
            Self::Workspace(error) => matches!(
                error.as_ref(),
                WorkspaceError::Io { .. } | WorkspaceError::SourceChanged { .. }
            ),
            Self::Store(error) => matches!(
                error.as_ref(),
                GenerationStoreError::Io { .. }
                    | GenerationStoreError::WriterLeaseUnavailable { .. }
            ),
            Self::SourceState(error) => matches!(
                error.as_ref(),
                SourceStateError::Store(
                    store_error
                ) if matches!(
                    store_error.as_ref(),
                    GenerationStoreError::Io { .. }
                        | GenerationStoreError::WriterLeaseUnavailable { .. }
                )
            ),
            Self::UnsupportedContractVersion { .. }
            | Self::WorkspaceViewRequired
            | Self::WorkspaceMismatch { .. }
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
            | Self::Manifest(_)
            | Self::Json(_) => false,
        }
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedContractVersion { actual, expected } => write!(
                formatter,
                "reindex contract version {actual} is unsupported; expected {expected}"
            ),
            Self::WorkspaceViewRequired => formatter.write_str(
                "ChangeSet reindexing requires the authoritative WorkspaceView at to_revision",
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
            | Self::Scan(error)
            | Self::Query(error)
            | Self::Projection(error) => fmt::Display::fmt(error, formatter),
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::Contract(error) => fmt::Display::fmt(error, formatter),
            Self::Diagnostic(error) => fmt::Display::fmt(error, formatter),
            Self::Workspace(error) => fmt::Display::fmt(error, formatter),
            Self::ReferenceGraph(error) => fmt::Display::fmt(error, formatter),
            Self::Analysis(error) => fmt::Display::fmt(error, formatter),
            Self::SourceState(error) => fmt::Display::fmt(error, formatter),
            Self::Store(error) => fmt::Display::fmt(error, formatter),
            Self::Manifest(error) => fmt::Display::fmt(error, formatter),
            Self::Json(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl StdError for PipelineError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Configuration(error)
            | Self::Scan(error)
            | Self::Query(error)
            | Self::Projection(error) => Some(error.as_ref()),
            Self::Budget(error) => Some(error),
            Self::Contract(error) => Some(error),
            Self::Diagnostic(error) => Some(error),
            Self::Workspace(error) => Some(error.as_ref()),
            Self::ReferenceGraph(error) => Some(error.as_ref()),
            Self::Analysis(error) => Some(error.as_ref()),
            Self::SourceState(error) => Some(error.as_ref()),
            Self::Store(error) => Some(error.as_ref()),
            Self::Manifest(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Allocation { source, .. } => Some(source),
            Self::UnsupportedContractVersion { .. }
            | Self::WorkspaceViewRequired
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
    use crate::analysis::{AnalyzedSource, SearchFacts};
    use unity_asset::{AssetLoadLimits, BudgetedSourceBytes};

    fn tier_zero_analysis(path: &str) -> AssetAnalysis {
        AssetAnalysis::new(
            AnalyzedSource {
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

    #[test]
    fn workspace_sniff_rejects_source_bytes_from_another_budget_domain() {
        let mut source_budget = AssetLoadBudget::default();
        let bytes =
            BudgetedSourceBytes::from_vec(b"%YAML 1.1\n".to_vec(), &mut source_budget).unwrap();
        let source = ReadSource {
            rel_path: "Assets/Unknown.data".to_owned(),
            abs_path: PathBuf::from("Assets/Unknown.data"),
            name: "Unknown.data".to_owned(),
            kind: SearchKind::File,
            guid: None,
            bytes: Some(bytes),
            meta_bytes: None,
            length: 10,
            content_identity: DigestV1::hash_bytes(b"%YAML 1.1\n"),
            hints: SourceHints {
                asset: FileHint {
                    size: 10,
                    mtime_ms: None,
                },
                meta: None,
            },
            unchanged: false,
        };

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
}
