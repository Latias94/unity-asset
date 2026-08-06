use std::collections::{BTreeMap, BTreeSet, TryReserveError};
use std::error::Error as StdError;
use std::fmt;
use std::mem::size_of;

use unity_asset::reference::{
    BinaryExternalReference, RawReferenceTarget, ReferenceFact, ReferenceGraph,
    ReferenceGraphError, ReferenceGuid, ReferenceResolution, ReferenceTruncationKind,
};
use unity_asset::workspace::{WorkspaceError, WorkspaceSource, WorkspaceView};
use unity_asset::{
    AssetLoadBudget, BudgetError, ContractError, Diagnostic, DiagnosticError, DiagnosticSeverity,
    FieldPath, FieldPathSegment, ObjectAddress, RevisionedObjectHandle, SourceFingerprint,
    SourceId, SourceKind, SourceLocator, UnityClass, UnityValue, WorkspaceId, WorkspaceRevision,
};
use unity_asset_search_core::{SearchKind, TryToTermsError, try_to_terms};

use crate::analysis::{
    AnalysisMetrics, AnalysisTruncation, AnalysisTruncationKind, AnalyzedSource, AssetAnalysis,
    BinaryExternalProjection, ContainerEntryFact, GuidProjection, RawReferenceProjection,
    ReferenceDependencyKey, ReferenceProjectionFact, ReferenceResolutionProjection, SearchFacts,
    WorkspaceGraphInputs, WorkspaceObjectFact,
};
use crate::scan::ReadSource;

const PAYLOAD_UNAVAILABLE: &str = "ANALYSIS_PAYLOAD_UNAVAILABLE";
const WORKSPACE_UNAVAILABLE: &str = "ANALYSIS_WORKSPACE_UNAVAILABLE";
const WORKSPACE_OBJECT_FAILED: &str = "ANALYSIS_WORKSPACE_OBJECT_FAILED";
const GRAPH_PARTIAL: &str = "ANALYSIS_REFERENCE_GRAPH_PARTIAL";
const TEXT_NOT_UTF8: &str = "ANALYSIS_TEXT_NOT_UTF8";
const HIERARCHY_CYCLE: &str = "ANALYSIS_HIERARCHY_CYCLE";
const HIERARCHY_BTREE_MAX_NODE_SLOTS: usize = 32;
const HIERARCHY_BTREE_NODE_METADATA_WORDS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AnalyzerLimits {
    pub(crate) max_text_bytes: u64,
    pub(crate) max_workspace_objects: u64,
    pub(crate) max_unity_values: u64,
    pub(crate) max_content_term_bytes: u64,
    pub(crate) max_hierarchy_paths: u64,
    pub(crate) max_hierarchy_depth: u32,
    pub(crate) max_script_symbols: u64,
    pub(crate) max_referenced_script_guids: u64,
    pub(crate) max_container_entries: u64,
    pub(crate) max_reference_facts: u64,
}

impl Default for AnalyzerLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: 8 * 1024 * 1024,
            max_workspace_objects: 200_000,
            max_unity_values: 2_000_000,
            max_content_term_bytes: 4 * 1024 * 1024,
            max_hierarchy_paths: 100_000,
            max_hierarchy_depth: 128,
            max_script_symbols: 4_096,
            max_referenced_script_guids: 65_536,
            max_container_entries: 100_000,
            max_reference_facts: 1_000_000,
        }
    }
}

/// Revision-bound data shared by every root analyzed in one indexing run.
///
/// Building this context never constructs a second reference graph. It only
/// indexes stable workspace handles and graph fact ordinals by their logical
/// root source.
pub(crate) struct WorkspaceAnalysisContext<'view> {
    view: &'view dyn WorkspaceView,
    graph: &'view ReferenceGraph,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    sources: BTreeMap<SourceId, WorkspaceSource>,
    root_by_source: BTreeMap<SourceId, SourceId>,
    objects_by_root: BTreeMap<SourceId, Vec<RevisionedObjectHandle>>,
    facts_by_root: BTreeMap<SourceId, Vec<usize>>,
}

impl fmt::Debug for WorkspaceAnalysisContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceAnalysisContext")
            .field("workspace", &self.workspace)
            .field("revision", &self.revision)
            .field("source_count", &self.sources.len())
            .field("root_count", &self.objects_by_root.len())
            .field("graph_fact_count", &self.graph.facts().len())
            .finish()
    }
}

impl<'view> WorkspaceAnalysisContext<'view> {
    pub(crate) fn build(
        view: &'view dyn WorkspaceView,
        graph: &'view ReferenceGraph,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, AnalysisError> {
        if graph.workspace_id() != view.workspace_id() || graph.revision() != view.revision() {
            return Err(AnalysisError::ContextMismatch {
                view_workspace: view.workspace_id(),
                view_revision: view.revision(),
                graph_workspace: graph.workspace_id(),
                graph_revision: graph.revision(),
            });
        }

        let source_list = view.sources(budget)?;
        let mut sources = BTreeMap::new();
        for source in source_list {
            charge_entry::<(SourceId, WorkspaceSource)>(budget)?;
            sources.insert(source.id(), source);
        }

        let mut root_by_source = BTreeMap::new();
        for source in sources.keys().copied() {
            let root = resolve_root_source(source, &sources)?;
            charge_entry::<(SourceId, SourceId)>(budget)?;
            root_by_source.insert(source, root);
        }

        let mut handles = view.objects(budget)?;
        handles.sort_unstable();
        let mut objects_by_root = BTreeMap::<SourceId, Vec<RevisionedObjectHandle>>::new();
        for handle in handles {
            let source = handle.object().source();
            let root = *root_by_source
                .get(&source)
                .ok_or(AnalysisError::UnknownWorkspaceSource(source))?;
            charge_entry::<RevisionedObjectHandle>(budget)?;
            push_fallible(
                objects_by_root.entry(root).or_default(),
                handle,
                "workspace analysis object handles",
            )?;
        }

        let mut facts_by_root = BTreeMap::<SourceId, Vec<usize>>::new();
        for (ordinal, fact) in graph.facts().iter().enumerate() {
            let source = fact.source().object().source();
            let root = *root_by_source
                .get(&source)
                .ok_or(AnalysisError::UnknownWorkspaceSource(source))?;
            charge_entry::<usize>(budget)?;
            push_fallible(
                facts_by_root.entry(root).or_default(),
                ordinal,
                "workspace analysis reference ordinals",
            )?;
        }

        Ok(Self {
            view,
            graph,
            workspace: view.workspace_id(),
            revision: view.revision(),
            sources,
            root_by_source,
            objects_by_root,
            facts_by_root,
        })
    }

    pub(crate) fn asset(
        &self,
        root: SourceId,
    ) -> Result<WorkspaceAssetInput<'_, 'view>, AnalysisError> {
        let source = self
            .sources
            .get(&root)
            .ok_or(AnalysisError::UnknownWorkspaceSource(root))?;
        if source.parent().is_some() || self.root_by_source.get(&root) != Some(&root) {
            return Err(AnalysisError::NotRootSource(root));
        }
        Ok(WorkspaceAssetInput {
            context: self,
            root,
        })
    }

    pub(crate) fn asset_for_analysis(
        &self,
        analysis: &AssetAnalysis,
    ) -> Result<WorkspaceAssetInput<'_, 'view>, AnalysisError> {
        let root = analysis
            .source
            .workspace_source
            .ok_or(AnalysisError::CachedAnalysisHasNoWorkspaceSource)?;
        self.asset(root)
    }

    fn source(&self, source: SourceId) -> Option<&WorkspaceSource> {
        self.sources.get(&source)
    }

    fn objects(&self, root: SourceId) -> &[RevisionedObjectHandle] {
        self.objects_by_root.get(&root).map_or(&[], Vec::as_slice)
    }

    fn facts(&self, root: SourceId) -> impl Iterator<Item = &ReferenceFact> {
        self.facts_by_root
            .get(&root)
            .into_iter()
            .flat_map(|ordinals| ordinals.iter())
            .filter_map(|ordinal| self.graph.facts().get(*ordinal))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct WorkspaceAssetInput<'context, 'view> {
    context: &'context WorkspaceAnalysisContext<'view>,
    root: SourceId,
}

impl fmt::Debug for WorkspaceAssetInput<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceAssetInput")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnalyzedAsset {
    pub(crate) analysis: AssetAnalysis,
    pub(crate) metrics: AnalysisMetrics,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AssetAnalyzer {
    limits: AnalyzerLimits,
}

impl AssetAnalyzer {
    pub(crate) const fn new(limits: AnalyzerLimits) -> Self {
        Self { limits }
    }

    pub(crate) fn analyze(
        &self,
        source: &ReadSource,
        workspace: Option<WorkspaceAssetInput<'_, '_>>,
        budget: &mut AssetLoadBudget,
    ) -> Result<AnalyzedAsset, AnalysisError> {
        if let Some(bytes) = source.bytes.as_ref() {
            bytes.validate_budget(budget)?;
        }
        if let Some(bytes) = source.meta_bytes.as_ref() {
            bytes.validate_budget(budget)?;
        }
        let analyzed_source = analyzed_source(source, workspace, budget)?;
        let mut state = AnalysisState::new(source, self.limits, budget)?;

        match workspace {
            Some(input) => self.analyze_workspace(input, &mut state, budget)?,
            None => self.analyze_memory(source, &mut state, budget)?,
        }

        if state.terms.truncated {
            let observed_at_least = state.terms.observed_at_least;
            state.add_truncation(AnalysisTruncation::new(
                AnalysisTruncationKind::ContentTerms,
                Some(self.limits.max_content_term_bytes),
                observed_at_least,
            ));
        }
        let content_terms = state.terms.finish(budget)?;
        let display_name = match state.primary_name.take() {
            Some(name) => name,
            None => {
                charge_string(&source.name, budget)?;
                source.name.clone()
            }
        };
        let name_terms = budgeted_terms(&display_name, budget)?;
        let hierarchy_paths = collect_string_set(
            std::mem::take(&mut state.hierarchy_paths),
            "analysis hierarchy paths",
            budget,
        )?;
        let script_symbols = collect_string_set(
            std::mem::take(&mut state.script_symbols),
            "analysis script symbols",
            budget,
        )?;
        let referenced_script_guids = collect_string_set(
            std::mem::take(&mut state.script_guids),
            "analysis referenced script GUIDs",
            budget,
        )?;

        let search = SearchFacts {
            display_name,
            path_terms: budgeted_terms(&source.rel_path, budget)?,
            name_terms,
            content_terms,
            hierarchy_paths,
            script_symbols,
            referenced_script_guids,
        };

        let analysis = AssetAnalysis::with_graph_inputs(
            analyzed_source,
            search,
            state.graph_inputs,
            state.references,
            state.container_entries,
            state.diagnostics,
            state.truncations,
            state.complete,
        );
        state.metrics.assets_analyzed = 1;
        finalize_metrics(&analysis, &mut state.metrics)?;

        Ok(AnalyzedAsset {
            analysis,
            metrics: state.metrics,
        })
    }

    /// Reprojects revision-sensitive graph facts without reopening or traversing the source.
    ///
    /// The cached source fingerprint must still match the current workspace root. A mismatch
    /// requires a full [`Self::analyze`] call because local search and container facts may have
    /// changed.
    pub(crate) fn refresh_graph_facts(
        &self,
        cached: &AssetAnalysis,
        input: WorkspaceAssetInput<'_, '_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<AnalyzedAsset, AnalysisError> {
        validate_cached_root(cached, input)?;
        let mut state = cached_refresh_state(cached, self.limits, budget)?;
        let (class_by_address, game_object_names) = graph_input_maps(&cached.graph_inputs, budget)?;
        self.project_references_and_hierarchy(
            input,
            &mut state,
            &class_by_address,
            &game_object_names,
            budget,
        )?;

        let mut search = clone_search_facts(&cached.search, budget)?;
        search.hierarchy_paths = collect_string_set(
            std::mem::take(&mut state.hierarchy_paths),
            "refreshed hierarchy paths",
            budget,
        )?;
        search.referenced_script_guids = collect_string_set(
            std::mem::take(&mut state.script_guids),
            "refreshed referenced script GUIDs",
            budget,
        )?;
        let source = clone_analyzed_source(&cached.source, budget)?;
        let analysis = AssetAnalysis::with_graph_inputs(
            source,
            search,
            state.graph_inputs,
            state.references,
            state.container_entries,
            state.diagnostics,
            state.truncations,
            state.complete,
        );
        state.metrics.assets_analyzed = 1;
        finalize_metrics(&analysis, &mut state.metrics)?;
        Ok(AnalyzedAsset {
            analysis,
            metrics: state.metrics,
        })
    }

    fn analyze_memory(
        &self,
        source: &ReadSource,
        state: &mut AnalysisState,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), AnalysisError> {
        let Some(bytes) = source.bytes.as_deref() else {
            state.mark_incomplete(
                diagnostic(
                    DiagnosticSeverity::Warning,
                    PAYLOAD_UNAVAILABLE,
                    format!(
                        "source payload was not retained; only identity fields were analyzed for {}",
                        source.rel_path
                    ),
                )?,
                Some(AnalysisTruncation::new(
                    AnalysisTruncationKind::PayloadUnavailable,
                    None,
                    source.length,
                )),
            );
            return Ok(());
        };

        match source.kind {
            SearchKind::Prefab
            | SearchKind::Scene
            | SearchKind::Material
            | SearchKind::AnimationClip
            | SearchKind::AnimatorController
            | SearchKind::Asset => {
                state.mark_incomplete(
                    diagnostic(
                        DiagnosticSeverity::Warning,
                        WORKSPACE_UNAVAILABLE,
                        format!(
                            "Unity source {} was not supplied through a frozen workspace view",
                            source.rel_path
                        ),
                    )?,
                    None,
                );
                Ok(())
            }
            SearchKind::Texture | SearchKind::Audio | SearchKind::BundleContainer => Ok(()),
            SearchKind::Script | SearchKind::Shader | SearchKind::File => {
                self.analyze_text(source, bytes, state, budget)
            }
        }
    }

    fn analyze_text(
        &self,
        source: &ReadSource,
        bytes: &[u8],
        state: &mut AnalysisState,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), AnalysisError> {
        let byte_limit = u64_to_usize(self.limits.max_text_bytes)?;
        let bounded_len = bytes.len().min(byte_limit);
        let mut candidate = &bytes[..bounded_len];
        let truncated = bounded_len < bytes.len();

        let text = match std::str::from_utf8(candidate) {
            Ok(text) => text,
            Err(error) if truncated && error.error_len().is_none() => {
                candidate = &candidate[..error.valid_up_to()];
                std::str::from_utf8(candidate)
                    .map_err(|_| AnalysisError::Invariant("UTF-8 valid prefix was rejected"))?
            }
            Err(error) => {
                if source.kind == SearchKind::File {
                    state.mark_incomplete(
                        diagnostic(
                            DiagnosticSeverity::Info,
                            TEXT_NOT_UTF8,
                            format!(
                                "file payload is not UTF-8 at byte {}; text facts were omitted for {}",
                                error.valid_up_to(),
                                source.rel_path
                            ),
                        )?,
                        None,
                    );
                    return Ok(());
                }
                state.mark_incomplete(
                    diagnostic(
                        DiagnosticSeverity::Warning,
                        TEXT_NOT_UTF8,
                        format!(
                            "expected text payload is not UTF-8 at byte {} for {}",
                            error.valid_up_to(),
                            source.rel_path
                        ),
                    )?,
                    None,
                );
                return Ok(());
            }
        };

        state.metrics.text_sources = state.metrics.text_sources.saturating_add(1);
        state.metrics.text_bytes_scanned = state
            .metrics
            .text_bytes_scanned
            .saturating_add(usize_to_u64(candidate.len())?);
        if truncated {
            state.add_truncation(AnalysisTruncation::new(
                AnalysisTruncationKind::TextBytes,
                Some(self.limits.max_text_bytes),
                source.length,
            ));
        }

        for line in text.lines() {
            budget.consume_entries(1)?;
            state.terms.append(line, budget)?;
            if source.kind == SearchKind::Script {
                for symbol in csharp_symbols_on_line(line).into_iter().flatten() {
                    charge_string(symbol, budget)?;
                    if !insert_bounded(
                        &mut state.script_symbols,
                        symbol.to_owned(),
                        self.limits.max_script_symbols,
                    )? {
                        state.add_truncation(AnalysisTruncation::new(
                            AnalysisTruncationKind::ScriptSymbols,
                            Some(self.limits.max_script_symbols),
                            self.limits.max_script_symbols.saturating_add(1),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn analyze_workspace(
        &self,
        input: WorkspaceAssetInput<'_, '_>,
        state: &mut AnalysisState,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), AnalysisError> {
        let context = input.context;
        let handles = context.objects(input.root);
        let object_limit = u64_to_usize(self.limits.max_workspace_objects)?;
        let mut graph_inputs_complete = handles.len() <= object_limit;
        if handles.len() > object_limit {
            state.add_truncation(AnalysisTruncation::new(
                AnalysisTruncationKind::WorkspaceObjects,
                Some(self.limits.max_workspace_objects),
                usize_to_u64(handles.len())?,
            ));
        }

        if !context.graph.is_complete() {
            state.mark_incomplete(
                diagnostic(
                    DiagnosticSeverity::Warning,
                    GRAPH_PARTIAL,
                    format!(
                        "reference graph coverage is partial for workspace revision {}",
                        context.revision
                    ),
                )?,
                None,
            );
            for truncation in context.graph.coverage().truncations() {
                let kind = match truncation.kind() {
                    ReferenceTruncationKind::Nodes => AnalysisTruncationKind::ReferenceGraphNodes,
                    ReferenceTruncationKind::Facts => AnalysisTruncationKind::ReferenceGraphFacts,
                };
                state.add_truncation(AnalysisTruncation::new(
                    kind,
                    Some(truncation.limit()),
                    truncation.observed(),
                ));
            }
        }

        let mut class_by_address = BTreeMap::<ObjectAddress, i32>::new();
        let mut game_object_names = BTreeMap::<ObjectAddress, String>::new();
        let mut value_traversal = ValueTraversal::new(self.limits.max_unity_values);

        for handle in handles.iter().take(object_limit) {
            budget.consume_entries(1)?;
            let address = match context.graph.address(handle) {
                Ok(address) => {
                    charge_address(address, budget)?;
                    Some(address.clone())
                }
                Err(ReferenceGraphError::ObjectNotIndexed) if !context.graph.is_complete() => {
                    graph_inputs_complete = false;
                    None
                }
                Err(error) => return Err(error.into()),
            };
            let object = match context.view.read_object(handle, budget) {
                Ok(object) => object,
                Err(error) if !matches!(&error, WorkspaceError::Budget(_)) => {
                    graph_inputs_complete = false;
                    let mut diagnostic = diagnostic(
                        DiagnosticSeverity::Warning,
                        WORKSPACE_OBJECT_FAILED,
                        format!(
                            "workspace object could not be inspected: {}",
                            bounded_error_message(&error)
                        ),
                    )?;
                    if let Some(address) = address {
                        diagnostic = diagnostic.at_address(address);
                    }
                    state.mark_incomplete(diagnostic, None);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };

            match handle.object().source().kind() {
                SourceKind::Yaml => {
                    state.metrics.yaml_documents = state.metrics.yaml_documents.saturating_add(1);
                }
                SourceKind::SerializedFile => {
                    state.metrics.binary_objects = state.metrics.binary_objects.saturating_add(1);
                }
                SourceKind::AssetBundle
                | SourceKind::WebFile
                | SourceKind::Archive
                | SourceKind::StreamedResource => {}
            }

            let class = object.class();
            state.terms.append(class.class_name(), budget)?;
            if let Some(name) = class.name().filter(|name| !name.trim().is_empty()) {
                state.terms.append(name, budget)?;
                if state.primary_name.is_none() {
                    charge_string(name, budget)?;
                    state.primary_name = Some(name.to_owned());
                }
                if class.class_id() == 1
                    && let Some(address) = address.as_ref()
                {
                    charge_string(name, budget)?;
                    charge_address(address, budget)?;
                    game_object_names.insert(address.clone(), name.to_owned());
                }
            }
            if let Some(address) = address {
                class_by_address.insert(address, class.class_id());
            }

            self.walk_class(class, state, &mut value_traversal, budget)?;
        }

        if value_traversal.truncated {
            state.add_truncation(AnalysisTruncation::new(
                AnalysisTruncationKind::UnityValues,
                Some(self.limits.max_unity_values),
                self.limits.max_unity_values.saturating_add(1),
            ));
        }
        state.metrics.unity_values_visited = value_traversal.visited;
        if !graph_inputs_complete {
            state.add_truncation(AnalysisTruncation::new(
                AnalysisTruncationKind::GraphRefreshInputs,
                None,
                usize_to_u64(handles.len())?,
            ));
        }
        state.graph_inputs = build_graph_inputs(
            &class_by_address,
            &game_object_names,
            graph_inputs_complete,
            budget,
        )?;

        self.project_references_and_hierarchy(
            input,
            state,
            &class_by_address,
            &game_object_names,
            budget,
        )
    }

    fn walk_class(
        &self,
        class: &UnityClass,
        state: &mut AnalysisState,
        traversal: &mut ValueTraversal,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), AnalysisError> {
        let mut frames = Vec::<ValueFrame<'_>>::new();
        let remaining = traversal.remaining();
        if class.properties().len() > remaining {
            traversal.truncated = true;
        }
        for (field, value) in class.properties().iter().take(remaining).rev() {
            if class.class_id() == 142 && field == "m_Container" {
                self.collect_container_entries(value, state, traversal, budget)?;
            } else {
                push_value_frame(
                    &mut frames,
                    ValueFrame {
                        value,
                        field,
                        inside_script: field == "m_Script",
                        depth: 0,
                    },
                    budget,
                )?;
            }
        }

        while let Some(frame) = frames.pop() {
            if !traversal.visit(frame.depth, budget)? {
                break;
            }
            match frame.value {
                UnityValue::String(value) => {
                    if is_indexed_string_field(frame.field) {
                        state.terms.append(value, budget)?;
                    }
                    if frame.inside_script
                        && frame.field.eq_ignore_ascii_case("guid")
                        && let Some(guid) = normalize_guid(value, budget)?
                    {
                        self.insert_script_guid(guid, state)?;
                    }
                }
                UnityValue::Integer(value) if is_indexed_numeric_field(frame.field) => {
                    state.terms.append(&value.to_string(), budget)?;
                }
                UnityValue::Unsigned(value) if is_indexed_numeric_field(frame.field) => {
                    state.terms.append(&value.to_string(), budget)?;
                }
                UnityValue::Array(values) => {
                    push_array_children(
                        &mut frames,
                        values,
                        frame.field,
                        frame.inside_script,
                        frame.depth,
                        traversal,
                        budget,
                    )?;
                }
                UnityValue::Object(values) => {
                    let child_depth = frame.depth.saturating_add(1);
                    let remaining = traversal.remaining();
                    if values.len() > remaining {
                        traversal.truncated = true;
                    }
                    for (field, value) in values.iter().take(remaining).rev() {
                        push_value_frame(
                            &mut frames,
                            ValueFrame {
                                value,
                                field,
                                inside_script: frame.inside_script || field == "m_Script",
                                depth: child_depth,
                            },
                            budget,
                        )?;
                    }
                }
                UnityValue::Null
                | UnityValue::Bool(_)
                | UnityValue::Integer(_)
                | UnityValue::Unsigned(_)
                | UnityValue::Float(_)
                | UnityValue::Bytes(_) => {}
            }
        }
        Ok(())
    }

    fn collect_container_entries(
        &self,
        value: &UnityValue,
        state: &mut AnalysisState,
        traversal: &mut ValueTraversal,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), AnalysisError> {
        if !traversal.visit(0, budget)? {
            return Ok(());
        }
        let UnityValue::Array(items) = value else {
            return Ok(());
        };
        let entry_limit = u64_to_usize(self.limits.max_container_entries)?;
        if items.len() > entry_limit {
            state.add_truncation(AnalysisTruncation::new(
                AnalysisTruncationKind::ContainerEntries,
                Some(self.limits.max_container_entries),
                usize_to_u64(items.len())?,
            ));
        }

        for item in items.iter().take(entry_limit) {
            if !traversal.visit(1, budget)? {
                break;
            }
            let Some((asset_path, target)) = container_pair(item) else {
                continue;
            };
            if !traversal.visit(2, budget)? {
                break;
            }
            let Some((file_id, path_id)) = find_pptr(target, 2, traversal, budget)? else {
                continue;
            };
            state.terms.append(asset_path, budget)?;
            charge_string(asset_path, budget)?;
            charge_entry::<ContainerEntryFact>(budget)?;
            push_fallible(
                &mut state.container_entries,
                ContainerEntryFact {
                    asset_path: asset_path.to_owned(),
                    file_id,
                    path_id,
                },
                "analysis container entries",
            )?;
        }
        Ok(())
    }

    fn project_references_and_hierarchy(
        &self,
        input: WorkspaceAssetInput<'_, '_>,
        state: &mut AnalysisState,
        class_by_address: &BTreeMap<ObjectAddress, i32>,
        game_object_names: &BTreeMap<ObjectAddress, String>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), AnalysisError> {
        let fact_count = input
            .context
            .facts_by_root
            .get(&input.root)
            .map_or(0, Vec::len);
        let reference_limit = u64_to_usize(self.limits.max_reference_facts)?;
        if fact_count > reference_limit {
            state.add_truncation(AnalysisTruncation::new(
                AnalysisTruncationKind::ReferenceFacts,
                Some(self.limits.max_reference_facts),
                usize_to_u64(fact_count)?,
            ));
        }
        if !state.references.is_empty() {
            return Err(AnalysisError::Invariant(
                "reference projection state must be empty before graph projection",
            ));
        }
        state.references = reserve_retained_vec(
            fact_count.min(reference_limit),
            "analysis reference facts",
            budget,
        )?;

        let mut transform_game_object = BTreeMap::<ObjectAddress, ObjectAddress>::new();
        let mut transform_parent = BTreeMap::<ObjectAddress, ObjectAddress>::new();

        for fact in input.context.facts(input.root).take(reference_limit) {
            budget.consume_entries(1)?;
            let projected = project_reference(fact, input.context.graph, class_by_address, budget)?;
            let source_address = &projected.source_object;
            let resolved_target = match &projected.resolution {
                ReferenceResolutionProjection::Resolved { target } => Some(target),
                ReferenceResolutionProjection::Null
                | ReferenceResolutionProjection::Unloaded { .. }
                | ReferenceResolutionProjection::Missing { .. }
                | ReferenceResolutionProjection::Ambiguous { .. }
                | ReferenceResolutionProjection::Invalid => None,
            };
            let terminal_field = last_field(projected.field_path.segments());

            if matches!(projected.source_class_id, Some(4 | 224))
                && let (Some(target), Some(field)) = (resolved_target, terminal_field)
            {
                if field == "m_GameObject" {
                    charge_entry::<(ObjectAddress, ObjectAddress)>(budget)?;
                    transform_game_object.insert(
                        clone_object_address(source_address, "transform source object", budget)?,
                        clone_object_address(target, "transform game object", budget)?,
                    );
                } else if field == "m_Father" {
                    charge_entry::<(ObjectAddress, ObjectAddress)>(budget)?;
                    transform_parent.insert(
                        clone_object_address(source_address, "transform source object", budget)?,
                        clone_object_address(target, "transform parent object", budget)?,
                    );
                }
            }

            if path_contains_field(projected.field_path.segments(), "m_Script")
                && let Some(guid) = script_guid_from_raw(&projected.raw_target, budget)?
            {
                self.insert_script_guid(guid, state)?;
            }

            state.references.push(projected);
        }

        self.build_hierarchy_paths(
            state,
            game_object_names,
            &transform_game_object,
            &transform_parent,
            budget,
        )
    }

    fn build_hierarchy_paths(
        &self,
        state: &mut AnalysisState,
        game_object_names: &BTreeMap<ObjectAddress, String>,
        transform_game_object: &BTreeMap<ObjectAddress, ObjectAddress>,
        transform_parent: &BTreeMap<ObjectAddress, ObjectAddress>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), AnalysisError> {
        let depth_capacity = transform_game_object.len().min(
            usize::try_from(self.limits.max_hierarchy_depth)
                .unwrap_or(usize::MAX)
                .max(1),
        );
        let mut lineage = reserve_entry_vec::<(&ObjectAddress, &str)>(
            depth_capacity,
            "transform hierarchy traversal",
            budget,
        )?;

        for transform in transform_game_object.keys() {
            if usize_to_u64(state.hierarchy_paths.len())? >= self.limits.max_hierarchy_paths {
                state.add_truncation(AnalysisTruncation::new(
                    AnalysisTruncationKind::HierarchyPaths,
                    Some(self.limits.max_hierarchy_paths),
                    self.limits.max_hierarchy_paths.saturating_add(1),
                ));
                break;
            }

            let mut current = Some(transform);
            lineage.clear();
            let mut depth = 0_u32;
            while let Some(node) = current {
                if lineage.iter().any(|(seen, _)| *seen == node) {
                    state.mark_incomplete(
                        diagnostic(
                            DiagnosticSeverity::Warning,
                            HIERARCHY_CYCLE,
                            "transform hierarchy contains a cycle",
                        )?,
                        None,
                    );
                    break;
                }
                let Some(game_object) = transform_game_object.get(node) else {
                    break;
                };
                let Some(name) = game_object_names.get(game_object) else {
                    break;
                };
                lineage.push((node, name));
                depth = depth.saturating_add(1);
                if depth >= self.limits.max_hierarchy_depth {
                    if transform_parent.contains_key(node) {
                        state.add_truncation(AnalysisTruncation::new(
                            AnalysisTruncationKind::HierarchyDepth,
                            Some(u64::from(self.limits.max_hierarchy_depth)),
                            u64::from(depth).saturating_add(1),
                        ));
                    }
                    break;
                }
                current = transform_parent.get(node);
            }
            if lineage.is_empty() {
                continue;
            }
            let path = hierarchy_path(&lineage, budget)?;
            if state.hierarchy_paths.contains(&path) {
                continue;
            }
            charge_hierarchy_set_entry(budget)?;
            if !state.hierarchy_paths.insert(path) {
                return Err(AnalysisError::Invariant(
                    "hierarchy path changed ordering during insertion",
                ));
            }
        }
        Ok(())
    }

    fn insert_script_guid(
        &self,
        guid: String,
        state: &mut AnalysisState,
    ) -> Result<(), AnalysisError> {
        if !insert_bounded(
            &mut state.script_guids,
            guid,
            self.limits.max_referenced_script_guids,
        )? {
            state.add_truncation(AnalysisTruncation::new(
                AnalysisTruncationKind::ReferencedScriptGuids,
                Some(self.limits.max_referenced_script_guids),
                self.limits.max_referenced_script_guids.saturating_add(1),
            ));
        }
        Ok(())
    }
}

struct AnalysisState {
    metrics: AnalysisMetrics,
    diagnostics: Vec<Diagnostic>,
    truncations: Vec<AnalysisTruncation>,
    references: Vec<ReferenceProjectionFact>,
    container_entries: Vec<ContainerEntryFact>,
    hierarchy_paths: BTreeSet<String>,
    script_symbols: BTreeSet<String>,
    script_guids: BTreeSet<String>,
    primary_name: Option<String>,
    terms: TermCollector,
    graph_inputs: WorkspaceGraphInputs,
    complete: bool,
}

impl AnalysisState {
    fn new(
        source: &ReadSource,
        limits: AnalyzerLimits,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, AnalysisError> {
        let mut terms = TermCollector::new(limits.max_content_term_bytes);
        terms.append(&source.name, budget)?;
        Ok(Self {
            metrics: AnalysisMetrics {
                assets_visited: 1,
                ..AnalysisMetrics::default()
            },
            diagnostics: Vec::new(),
            truncations: Vec::new(),
            references: Vec::new(),
            container_entries: Vec::new(),
            hierarchy_paths: BTreeSet::new(),
            script_symbols: BTreeSet::new(),
            script_guids: BTreeSet::new(),
            primary_name: None,
            terms,
            graph_inputs: WorkspaceGraphInputs::default(),
            complete: true,
        })
    }

    fn mark_incomplete(&mut self, diagnostic: Diagnostic, truncation: Option<AnalysisTruncation>) {
        self.complete = false;
        self.diagnostics.push(diagnostic);
        if let Some(truncation) = truncation {
            self.add_truncation(truncation);
        }
    }

    fn add_truncation(&mut self, truncation: AnalysisTruncation) {
        self.complete = false;
        self.truncations.push(truncation);
    }
}

struct TermCollector {
    raw: String,
    max_bytes: u64,
    observed_at_least: u64,
    truncated: bool,
}

impl TermCollector {
    const fn new(max_bytes: u64) -> Self {
        Self {
            raw: String::new(),
            max_bytes,
            observed_at_least: 0,
            truncated: false,
        }
    }

    fn append(&mut self, value: &str, budget: &mut AssetLoadBudget) -> Result<(), AnalysisError> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(());
        }
        let separator = u64::from(!self.raw.is_empty());
        let value_len = usize_to_u64(value.len())?;
        self.observed_at_least = self
            .observed_at_least
            .saturating_add(separator)
            .saturating_add(value_len);
        let current = usize_to_u64(self.raw.len())?;
        if current >= self.max_bytes {
            self.truncated = true;
            return Ok(());
        }
        let available = self.max_bytes - current;
        let separator_len = usize::from(!self.raw.is_empty() && available > 0);
        let available_for_value = available.saturating_sub(separator_len as u64);
        let mut take = value.len().min(u64_to_usize(available_for_value)?);
        while take > 0 && !value.is_char_boundary(take) {
            take -= 1;
        }
        let appended = separator_len.saturating_add(take);
        if appended > 0 {
            budget.consume_bytes(usize_to_u64(appended)?)?;
            self.raw
                .try_reserve_exact(appended)
                .map_err(|source| AnalysisError::Allocation {
                    resource: "analysis content terms",
                    requested: appended,
                    source,
                })?;
            if separator_len == 1 {
                self.raw.push(' ');
            }
            self.raw.push_str(&value[..take]);
        }
        if take < value.len() {
            self.truncated = true;
        }
        Ok(())
    }

    fn finish(self, budget: &mut AssetLoadBudget) -> Result<String, AnalysisError> {
        budgeted_terms(&self.raw, budget)
    }
}

struct ValueTraversal {
    visited: u64,
    maximum: u64,
    truncated: bool,
}

impl ValueTraversal {
    const fn new(maximum: u64) -> Self {
        Self {
            visited: 0,
            maximum,
            truncated: false,
        }
    }

    fn visit(&mut self, depth: u32, budget: &mut AssetLoadBudget) -> Result<bool, AnalysisError> {
        if self.visited >= self.maximum {
            self.truncated = true;
            return Ok(false);
        }
        budget.observe_depth(depth)?;
        budget.consume_entries(1)?;
        self.visited = self.visited.saturating_add(1);
        Ok(true)
    }

    fn remaining(&self) -> usize {
        usize::try_from(self.maximum.saturating_sub(self.visited)).unwrap_or(usize::MAX)
    }
}

#[derive(Clone, Copy)]
struct ValueFrame<'value> {
    value: &'value UnityValue,
    field: &'value str,
    inside_script: bool,
    depth: u32,
}

fn push_array_children<'value>(
    frames: &mut Vec<ValueFrame<'value>>,
    values: &'value [UnityValue],
    field: &'value str,
    inside_script: bool,
    parent_depth: u32,
    traversal: &mut ValueTraversal,
    budget: &mut AssetLoadBudget,
) -> Result<(), AnalysisError> {
    let remaining = traversal.remaining();
    if values.len() > remaining {
        traversal.truncated = true;
    }
    let child_depth = parent_depth.saturating_add(1);
    for value in values.iter().take(remaining).rev() {
        push_value_frame(
            frames,
            ValueFrame {
                value,
                field,
                inside_script,
                depth: child_depth,
            },
            budget,
        )?;
    }
    Ok(())
}

fn push_value_frame<'value>(
    frames: &mut Vec<ValueFrame<'value>>,
    frame: ValueFrame<'value>,
    budget: &mut AssetLoadBudget,
) -> Result<(), AnalysisError> {
    charge_entry::<ValueFrame<'value>>(budget)?;
    push_fallible(frames, frame, "analysis value traversal stack")
}

fn container_pair(value: &UnityValue) -> Option<(&str, &UnityValue)> {
    match value {
        UnityValue::Array(pair) if pair.len() == 2 => Some((pair[0].as_str()?, &pair[1])),
        UnityValue::Object(pair) => Some((
            pair.get("first")?.as_str()?,
            pair.get("second").or_else(|| pair.get("value"))?,
        )),
        _ => None,
    }
}

fn find_pptr(
    root: &UnityValue,
    root_depth: u32,
    traversal: &mut ValueTraversal,
    budget: &mut AssetLoadBudget,
) -> Result<Option<(i32, i64)>, AnalysisError> {
    let mut stack = Vec::new();
    charge_entry::<(&UnityValue, u32)>(budget)?;
    push_fallible(
        &mut stack,
        (root, root_depth),
        "container pointer traversal stack",
    )?;
    while let Some((value, depth)) = stack.pop() {
        if !traversal.visit(depth, budget)? {
            return Ok(None);
        }
        match value {
            UnityValue::Object(object) => {
                let file_id = object
                    .get("fileID")
                    .or_else(|| object.get("m_FileID"))
                    .and_then(UnityValue::as_i64)
                    .and_then(|value| i32::try_from(value).ok());
                let path_id = object
                    .get("pathID")
                    .or_else(|| object.get("m_PathID"))
                    .and_then(UnityValue::as_i64);
                if let (Some(file_id), Some(path_id)) = (file_id, path_id) {
                    if !traversal.visit(depth.saturating_add(1), budget)?
                        || !traversal.visit(depth.saturating_add(1), budget)?
                    {
                        return Ok(None);
                    }
                    return Ok(Some((file_id, path_id)));
                }
                let child_depth = depth.saturating_add(1);
                let remaining = traversal.remaining();
                if object.len() > remaining {
                    traversal.truncated = true;
                }
                for child in object.values().take(remaining).rev() {
                    charge_entry::<(&UnityValue, u32)>(budget)?;
                    push_fallible(
                        &mut stack,
                        (child, child_depth),
                        "container pointer traversal stack",
                    )?;
                }
            }
            UnityValue::Array(values) => {
                let child_depth = depth.saturating_add(1);
                let remaining = traversal.remaining();
                if values.len() > remaining {
                    traversal.truncated = true;
                }
                for child in values.iter().take(remaining).rev() {
                    charge_entry::<(&UnityValue, u32)>(budget)?;
                    push_fallible(
                        &mut stack,
                        (child, child_depth),
                        "container pointer traversal stack",
                    )?;
                }
            }
            UnityValue::Null
            | UnityValue::Bool(_)
            | UnityValue::Integer(_)
            | UnityValue::Unsigned(_)
            | UnityValue::Float(_)
            | UnityValue::String(_)
            | UnityValue::Bytes(_) => {}
        }
    }
    Ok(None)
}

fn project_reference(
    fact: &ReferenceFact,
    graph: &ReferenceGraph,
    class_by_address: &BTreeMap<ObjectAddress, i32>,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceProjectionFact, AnalysisError> {
    let source = graph.address(fact.source())?;
    let source_class_id = class_by_address.get(source).copied();
    let source = clone_object_address(source, "reference source object", budget)?;
    let field_path = clone_field_path(fact.field_path(), "reference field path", budget)?;
    let raw_target = project_raw_reference(fact.raw_target(), budget)?;
    let diagnostics = clone_reference_diagnostics(fact.diagnostics(), fact.resolution(), budget)?;
    let resolution = match fact.resolution() {
        ReferenceResolution::Null => ReferenceResolutionProjection::Null,
        ReferenceResolution::Resolved(target) => ReferenceResolutionProjection::Resolved {
            target: clone_object_address(
                graph.address(target)?,
                "resolved reference target",
                budget,
            )?,
        },
        ReferenceResolution::Unloaded { source } => ReferenceResolutionProjection::Unloaded {
            source: source
                .as_ref()
                .map(|source| clone_source_locator(source, "unloaded reference source", budget))
                .transpose()?,
        },
        ReferenceResolution::Missing { target } => ReferenceResolutionProjection::Missing {
            target: target
                .as_ref()
                .map(|target| clone_object_address(target, "missing reference target", budget))
                .transpose()?,
        },
        ReferenceResolution::Ambiguous { candidates } => ReferenceResolutionProjection::Ambiguous {
            candidates: clone_ambiguous_candidates(candidates, budget)?,
        },
        ReferenceResolution::Invalid { .. } => ReferenceResolutionProjection::Invalid,
    };
    let dependency_keys = dependency_keys(&raw_target, &resolution, budget)?;
    Ok(ReferenceProjectionFact {
        source_object: source,
        source_class_id,
        field_path,
        raw_target,
        resolution,
        diagnostics,
        dependency_keys,
    })
}

fn clone_reference_diagnostics(
    diagnostics: &[Diagnostic],
    resolution: &ReferenceResolution,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<Diagnostic>, AnalysisError> {
    let invalid_diagnostic = match resolution {
        ReferenceResolution::Invalid { diagnostic } => Some(diagnostic),
        ReferenceResolution::Null
        | ReferenceResolution::Resolved(_)
        | ReferenceResolution::Unloaded { .. }
        | ReferenceResolution::Missing { .. }
        | ReferenceResolution::Ambiguous { .. } => None,
    };
    let count = diagnostics
        .len()
        .checked_add(usize::from(invalid_diagnostic.is_some()))
        .ok_or(AnalysisError::ArithmeticOverflow(
            "reference diagnostic count",
        ))?;
    let mut cloned = reserve_retained_vec(count, "reference diagnostics", budget)?;
    for diagnostic in diagnostics {
        cloned.push(clone_diagnostic(diagnostic, budget)?);
    }
    if let Some(diagnostic) = invalid_diagnostic {
        cloned.push(clone_diagnostic(diagnostic, budget)?);
    }
    Ok(cloned)
}

fn clone_ambiguous_candidates(
    candidates: &[ObjectAddress],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ObjectAddress>, AnalysisError> {
    let mut cloned = reserve_retained_vec(candidates.len(), "ambiguous reference targets", budget)?;
    for candidate in candidates {
        cloned.push(clone_object_address(
            candidate,
            "ambiguous reference target",
            budget,
        )?);
    }
    cloned.sort_unstable();
    cloned.dedup();
    Ok(cloned)
}

fn project_raw_reference(
    raw: &RawReferenceTarget,
    budget: &mut AssetLoadBudget,
) -> Result<RawReferenceProjection, AnalysisError> {
    Ok(match raw {
        RawReferenceTarget::Binary {
            file_id,
            path_id,
            external,
        } => RawReferenceProjection::Binary {
            file_id: *file_id,
            path_id: *path_id,
            external: external
                .as_ref()
                .map(|external| project_binary_external(external, budget))
                .transpose()?,
        },
        RawReferenceTarget::Yaml {
            file_id,
            guid,
            type_id,
        } => RawReferenceProjection::Yaml {
            file_id: *file_id,
            guid: guid
                .as_ref()
                .map(|guid| {
                    Ok::<_, AnalysisError>(match guid {
                        ReferenceGuid::Parsed(guid) => GuidProjection::Parsed(*guid),
                        ReferenceGuid::Invalid(value) => GuidProjection::Invalid(clone_string(
                            value,
                            "invalid reference GUID",
                            budget,
                        )?),
                    })
                })
                .transpose()?,
            type_id: *type_id,
        },
    })
}

fn project_binary_external(
    external: &BinaryExternalReference,
    budget: &mut AssetLoadBudget,
) -> Result<BinaryExternalProjection, AnalysisError> {
    Ok(BinaryExternalProjection {
        index: external.index(),
        guid: external.guid(),
        type_id: external.type_id(),
        path: clone_string(external.path(), "binary external reference path", budget)?,
    })
}

fn dependency_keys(
    raw: &RawReferenceProjection,
    resolution: &ReferenceResolutionProjection,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ReferenceDependencyKey>, AnalysisError> {
    let raw_count = usize::from(matches!(
        raw,
        RawReferenceProjection::Binary {
            external: Some(BinaryExternalProjection { guid: Some(_), .. }),
            ..
        } | RawReferenceProjection::Yaml { guid: Some(_), .. }
    ));
    let resolution_count = match resolution {
        ReferenceResolutionProjection::Resolved { .. }
        | ReferenceResolutionProjection::Unloaded { source: Some(_) }
        | ReferenceResolutionProjection::Missing { target: Some(_) } => 1,
        ReferenceResolutionProjection::Ambiguous { candidates } => candidates.len(),
        ReferenceResolutionProjection::Null
        | ReferenceResolutionProjection::Unloaded { source: None }
        | ReferenceResolutionProjection::Missing { target: None }
        | ReferenceResolutionProjection::Invalid => 0,
    };
    let capacity =
        raw_count
            .checked_add(resolution_count)
            .ok_or(AnalysisError::ArithmeticOverflow(
                "reference dependency key count",
            ))?;
    let mut keys = reserve_retained_vec(capacity, "reference dependency keys", budget)?;
    match raw {
        RawReferenceProjection::Binary {
            path_id,
            external: Some(external),
            ..
        } => {
            if let Some(guid) = external.guid {
                keys.push(ReferenceDependencyKey::Guid {
                    guid: encode_guid(guid, "binary dependency GUID", budget)?,
                    file_id: Some(*path_id),
                });
            }
        }
        RawReferenceProjection::Yaml { file_id, guid, .. } => {
            if let Some(guid) = guid {
                keys.push(ReferenceDependencyKey::Guid {
                    guid: match guid {
                        GuidProjection::Parsed(guid) => {
                            encode_guid(*guid, "YAML dependency GUID", budget)?
                        }
                        GuidProjection::Invalid(value) => {
                            clone_ascii_lowercase(value, "invalid dependency GUID", budget)?
                        }
                    },
                    file_id: *file_id,
                });
            }
        }
        RawReferenceProjection::Binary { external: None, .. } => {}
    }
    match resolution {
        ReferenceResolutionProjection::Resolved { target } => {
            keys.push(ReferenceDependencyKey::Object {
                address: clone_object_address(target, "resolved dependency object", budget)?,
            });
        }
        ReferenceResolutionProjection::Unloaded {
            source: Some(source),
        } => {
            keys.push(ReferenceDependencyKey::Source {
                locator: clone_source_locator(source, "unloaded dependency source", budget)?,
            });
        }
        ReferenceResolutionProjection::Missing {
            target: Some(target),
        } => {
            keys.push(ReferenceDependencyKey::Object {
                address: clone_object_address(target, "missing dependency object", budget)?,
            });
        }
        ReferenceResolutionProjection::Ambiguous { candidates } => {
            for candidate in candidates {
                keys.push(ReferenceDependencyKey::Object {
                    address: clone_object_address(
                        candidate,
                        "ambiguous dependency object",
                        budget,
                    )?,
                });
            }
        }
        ReferenceResolutionProjection::Null
        | ReferenceResolutionProjection::Unloaded { source: None }
        | ReferenceResolutionProjection::Missing { target: None }
        | ReferenceResolutionProjection::Invalid => {}
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

fn script_guid_from_raw(
    raw: &RawReferenceProjection,
    budget: &mut AssetLoadBudget,
) -> Result<Option<String>, AnalysisError> {
    Ok(match raw {
        RawReferenceProjection::Binary {
            external: Some(external),
            ..
        } => external
            .guid
            .map(|guid| encode_guid(guid, "binary script GUID", budget))
            .transpose()?,
        RawReferenceProjection::Yaml {
            guid: Some(GuidProjection::Parsed(guid)),
            ..
        } => Some(encode_guid(*guid, "YAML script GUID", budget)?),
        RawReferenceProjection::Yaml {
            guid: Some(GuidProjection::Invalid(value)),
            ..
        } => normalize_guid(value, budget)?,
        RawReferenceProjection::Binary { external: None, .. }
        | RawReferenceProjection::Yaml { guid: None, .. } => None,
    })
}

fn last_field(segments: &[FieldPathSegment]) -> Option<&str> {
    segments.iter().rev().find_map(|segment| match segment {
        FieldPathSegment::Field(field) => Some(field.as_str()),
        FieldPathSegment::Index(_) => None,
    })
}

fn path_contains_field(segments: &[FieldPathSegment], expected: &str) -> bool {
    segments
        .iter()
        .any(|segment| matches!(segment, FieldPathSegment::Field(field) if field == expected))
}

fn validate_cached_root(
    cached: &AssetAnalysis,
    input: WorkspaceAssetInput<'_, '_>,
) -> Result<(), AnalysisError> {
    let cached_root = cached
        .source
        .workspace_source
        .ok_or(AnalysisError::CachedAnalysisHasNoWorkspaceSource)?;
    if cached_root != input.root {
        return Err(AnalysisError::CachedAnalysisRootMismatch {
            cached: cached_root,
            requested: input.root,
        });
    }
    let current = input
        .context
        .source(input.root)
        .ok_or(AnalysisError::UnknownWorkspaceSource(input.root))?;
    if cached.source.workspace_fingerprint != Some(current.fingerprint()) {
        return Err(AnalysisError::CachedAnalysisStale {
            root: input.root,
            cached: cached.source.workspace_fingerprint,
            current: current.fingerprint(),
        });
    }
    if !cached.graph_inputs.complete {
        return Err(AnalysisError::CachedGraphInputsIncomplete { root: input.root });
    }
    Ok(())
}

fn cached_refresh_state(
    cached: &AssetAnalysis,
    limits: AnalyzerLimits,
    budget: &mut AssetLoadBudget,
) -> Result<AnalysisState, AnalysisError> {
    let had_graph_evidence = cached.diagnostics.iter().any(is_graph_diagnostic)
        || cached
            .truncations
            .iter()
            .any(|truncation| is_graph_truncation(truncation.kind));
    let diagnostics = clone_diagnostics(
        cached
            .diagnostics
            .iter()
            .filter(|diagnostic| !is_graph_diagnostic(diagnostic)),
        budget,
    )?;
    let mut truncations = Vec::new();
    for truncation in cached
        .truncations
        .iter()
        .copied()
        .filter(|truncation| !is_graph_truncation(truncation.kind))
    {
        charge_entry::<AnalysisTruncation>(budget)?;
        push_fallible(&mut truncations, truncation, "cached analysis truncations")?;
    }
    let complete =
        cached.complete || (had_graph_evidence && diagnostics.is_empty() && truncations.is_empty());

    Ok(AnalysisState {
        metrics: AnalysisMetrics {
            assets_visited: 1,
            ..AnalysisMetrics::default()
        },
        diagnostics,
        truncations,
        references: Vec::new(),
        container_entries: clone_container_entries(&cached.container_entries, budget)?,
        hierarchy_paths: BTreeSet::new(),
        script_symbols: BTreeSet::new(),
        script_guids: BTreeSet::new(),
        primary_name: None,
        terms: TermCollector::new(limits.max_content_term_bytes),
        graph_inputs: clone_graph_inputs(&cached.graph_inputs, budget)?,
        complete,
    })
}

fn is_graph_diagnostic(diagnostic: &Diagnostic) -> bool {
    matches!(diagnostic.code(), GRAPH_PARTIAL | HIERARCHY_CYCLE)
}

const fn is_graph_truncation(kind: AnalysisTruncationKind) -> bool {
    matches!(
        kind,
        AnalysisTruncationKind::HierarchyPaths
            | AnalysisTruncationKind::HierarchyDepth
            | AnalysisTruncationKind::ReferencedScriptGuids
            | AnalysisTruncationKind::ReferenceFacts
            | AnalysisTruncationKind::ReferenceGraphNodes
            | AnalysisTruncationKind::ReferenceGraphFacts
    )
}

fn build_graph_inputs(
    class_by_address: &BTreeMap<ObjectAddress, i32>,
    game_object_names: &BTreeMap<ObjectAddress, String>,
    complete: bool,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceGraphInputs, AnalysisError> {
    let mut objects = Vec::new();
    for (address, class_id) in class_by_address {
        charge_entry::<WorkspaceObjectFact>(budget)?;
        charge_address(address, budget)?;
        let name = match game_object_names.get(address) {
            Some(name) => {
                charge_string(name, budget)?;
                Some(name.clone())
            }
            None => None,
        };
        push_fallible(
            &mut objects,
            WorkspaceObjectFact {
                address: address.clone(),
                class_id: *class_id,
                name,
            },
            "workspace graph refresh inputs",
        )?;
    }
    Ok(WorkspaceGraphInputs::new(objects, complete))
}

type GraphInputMaps = (
    BTreeMap<ObjectAddress, i32>,
    BTreeMap<ObjectAddress, String>,
);

fn graph_input_maps(
    inputs: &WorkspaceGraphInputs,
    budget: &mut AssetLoadBudget,
) -> Result<GraphInputMaps, AnalysisError> {
    let mut classes = BTreeMap::new();
    let mut names = BTreeMap::new();
    for object in &inputs.objects {
        charge_entry::<(ObjectAddress, i32)>(budget)?;
        charge_address(&object.address, budget)?;
        classes.insert(object.address.clone(), object.class_id);
        if let Some(name) = object.name.as_ref() {
            charge_entry::<(ObjectAddress, String)>(budget)?;
            charge_address(&object.address, budget)?;
            charge_string(name, budget)?;
            names.insert(object.address.clone(), name.clone());
        }
    }
    Ok((classes, names))
}

fn clone_analyzed_source(
    source: &AnalyzedSource,
    budget: &mut AssetLoadBudget,
) -> Result<AnalyzedSource, AnalysisError> {
    charge_string(&source.relative_path, budget)?;
    if let Some(guid) = source.guid.as_deref() {
        charge_string(guid, budget)?;
    }
    if let Some(locator) = source.locator.as_ref() {
        charge_source_locator(locator, budget)?;
    }
    Ok(source.clone())
}

fn clone_search_facts(
    search: &SearchFacts,
    budget: &mut AssetLoadBudget,
) -> Result<SearchFacts, AnalysisError> {
    charge_string(&search.display_name, budget)?;
    charge_string(&search.path_terms, budget)?;
    charge_string(&search.name_terms, budget)?;
    charge_string(&search.content_terms, budget)?;
    Ok(SearchFacts {
        display_name: search.display_name.clone(),
        path_terms: search.path_terms.clone(),
        name_terms: search.name_terms.clone(),
        content_terms: search.content_terms.clone(),
        hierarchy_paths: Vec::new(),
        script_symbols: clone_strings(&search.script_symbols, budget)?,
        referenced_script_guids: Vec::new(),
    })
}

fn clone_strings(
    values: &[String],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<String>, AnalysisError> {
    charge_entries::<String>(values.len(), budget)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(values.len())
        .map_err(|source| AnalysisError::Allocation {
            resource: "analysis string clone",
            requested: size_of::<String>().saturating_mul(values.len()),
            source,
        })?;
    for value in values {
        charge_string(value, budget)?;
        output.push(value.clone());
    }
    Ok(output)
}

fn clone_container_entries(
    entries: &[ContainerEntryFact],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ContainerEntryFact>, AnalysisError> {
    charge_entries::<ContainerEntryFact>(entries.len(), budget)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(entries.len())
        .map_err(|source| AnalysisError::Allocation {
            resource: "cached container entries",
            requested: size_of::<ContainerEntryFact>().saturating_mul(entries.len()),
            source,
        })?;
    for entry in entries {
        charge_string(&entry.asset_path, budget)?;
        output.push(entry.clone());
    }
    Ok(output)
}

fn clone_graph_inputs(
    inputs: &WorkspaceGraphInputs,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceGraphInputs, AnalysisError> {
    charge_entries::<WorkspaceObjectFact>(inputs.objects.len(), budget)?;
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(inputs.objects.len())
        .map_err(|source| AnalysisError::Allocation {
            resource: "cached workspace graph inputs",
            requested: size_of::<WorkspaceObjectFact>().saturating_mul(inputs.objects.len()),
            source,
        })?;
    for object in &inputs.objects {
        charge_address(&object.address, budget)?;
        if let Some(name) = object.name.as_deref() {
            charge_string(name, budget)?;
        }
        objects.push(object.clone());
    }
    Ok(WorkspaceGraphInputs::new(objects, inputs.complete))
}

fn clone_diagnostics<'diagnostic>(
    diagnostics: impl IntoIterator<Item = &'diagnostic Diagnostic>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<Diagnostic>, AnalysisError> {
    let mut output = Vec::new();
    for diagnostic in diagnostics {
        charge_entry::<Diagnostic>(budget)?;
        charge_string(diagnostic.code(), budget)?;
        charge_string(diagnostic.message(), budget)?;
        if let Some(address) = diagnostic.address() {
            charge_address(address, budget)?;
        }
        if let Some(path) = diagnostic.field_path() {
            let retained = path
                .retained_clone_bytes()
                .ok_or(AnalysisError::ArithmeticOverflow("diagnostic field path"))?;
            budget.consume_bytes(usize_to_u64(retained)?)?;
        }
        push_fallible(
            &mut output,
            diagnostic.clone(),
            "cached analysis diagnostics",
        )?;
    }
    Ok(output)
}

fn finalize_metrics(
    analysis: &AssetAnalysis,
    metrics: &mut AnalysisMetrics,
) -> Result<(), AnalysisError> {
    metrics.references_emitted = usize_to_u64(analysis.references.len())?;
    metrics.container_entries_emitted = usize_to_u64(analysis.container_entries.len())?;
    metrics.truncations_emitted = usize_to_u64(analysis.truncations.len())?;
    metrics.diagnostics_emitted = analysis.references.iter().try_fold(
        usize_to_u64(analysis.diagnostics.len())?,
        |total, reference| {
            total
                .checked_add(usize_to_u64(reference.diagnostics.len())?)
                .ok_or(AnalysisError::ArithmeticOverflow("analysis diagnostics"))
        },
    )?;
    Ok(())
}

fn analyzed_source(
    source: &ReadSource,
    workspace: Option<WorkspaceAssetInput<'_, '_>>,
    budget: &mut AssetLoadBudget,
) -> Result<AnalyzedSource, AnalysisError> {
    charge_string(&source.rel_path, budget)?;
    if let Some(guid) = source.guid.as_deref() {
        charge_string(guid, budget)?;
    }
    let workspace_source = workspace.and_then(|input| input.context.source(input.root));
    if let Some(workspace_source) = workspace_source {
        charge_locator(workspace_source, budget)?;
    }
    Ok(AnalyzedSource {
        relative_path: source.rel_path.clone(),
        content_digest: source.content_identity,
        length: source.length,
        search_kind: source.kind,
        guid: source.guid.clone(),
        workspace_source: workspace_source.map(WorkspaceSource::id),
        workspace_fingerprint: workspace_source.map(WorkspaceSource::fingerprint),
        locator: workspace_source.map(|source| source.locator().clone()),
    })
}

fn resolve_root_source(
    source: SourceId,
    sources: &BTreeMap<SourceId, WorkspaceSource>,
) -> Result<SourceId, AnalysisError> {
    let mut current = source;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current) {
            return Err(AnalysisError::SourceHierarchyCycle(source));
        }
        let descriptor = sources
            .get(&current)
            .ok_or(AnalysisError::UnknownWorkspaceSource(current))?;
        match descriptor.parent() {
            Some(parent) => current = parent,
            None => return Ok(current),
        }
    }
}

fn csharp_symbols_on_line(line: &str) -> [Option<&str>; 2] {
    let line = line.split_once("//").map_or(line, |(code, _)| code);
    let mut tokens = line
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '.'))
        })
        .filter(|token| !token.is_empty());
    let mut namespace = None;
    let mut type_name = None;
    while let Some(token) = tokens.next() {
        if token == "namespace"
            && let Some(candidate) = tokens.next()
            && is_qualified_identifier(candidate)
        {
            namespace = Some(candidate);
            continue;
        }
        if matches!(token, "class" | "struct" | "interface" | "enum" | "record")
            && let Some(candidate) = tokens.next()
            && is_identifier(candidate)
        {
            type_name = Some(candidate);
        }
    }
    [namespace, type_name]
}

fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_qualified_identifier(value: &str) -> bool {
    !value.is_empty() && value.split('.').all(is_identifier)
}

fn is_indexed_string_field(field: &str) -> bool {
    matches!(
        field,
        "m_Name"
            | "m_TagString"
            | "m_EditorClassIdentifier"
            | "m_ClassName"
            | "m_Namespace"
            | "m_AssemblyName"
            | "m_Path"
            | "m_PathName"
            | "m_AssetPath"
            | "m_Source"
            | "m_Text"
            | "guid"
    )
}

fn is_indexed_numeric_field(field: &str) -> bool {
    matches!(field, "fileID" | "m_FileID" | "pathID" | "m_PathID")
}

fn normalize_guid(
    value: &str,
    budget: &mut AssetLoadBudget,
) -> Result<Option<String>, AnalysisError> {
    let mut hex_digits = 0_usize;
    for _ in value.bytes().filter(|byte| byte.is_ascii_hexdigit()) {
        if hex_digits == 32 {
            return Ok(None);
        }
        hex_digits += 1;
    }
    if hex_digits != 32 {
        return Ok(None);
    }

    let mut normalized = reserve_string(32, "normalized script GUID", budget)?;
    for byte in value.bytes().filter(|byte| byte.is_ascii_hexdigit()) {
        normalized.push(char::from(byte.to_ascii_lowercase()));
    }
    Ok(Some(normalized))
}

fn insert_bounded(
    values: &mut BTreeSet<String>,
    value: String,
    maximum: u64,
) -> Result<bool, AnalysisError> {
    if values.contains(&value) {
        return Ok(true);
    }
    if usize_to_u64(values.len())? >= maximum {
        return Ok(false);
    }
    values.insert(value);
    Ok(true)
}

fn budgeted_terms(value: &str, budget: &mut AssetLoadBudget) -> Result<String, AnalysisError> {
    try_to_terms(value, |requested| {
        let requested = usize_to_u64(requested)?;
        budget.check_bytes(requested)?;
        budget.consume_bytes(requested)?;
        Ok(())
    })
    .map_err(|error| match error {
        TryToTermsError::ReserveHook { source, .. } => source,
        TryToTermsError::Allocation { requested, source } => AnalysisError::Allocation {
            resource: "analysis normalized terms",
            requested,
            source,
        },
    })
}

fn hierarchy_path(
    lineage: &[(&ObjectAddress, &str)],
    budget: &mut AssetLoadBudget,
) -> Result<String, AnalysisError> {
    let names_len = lineage.iter().try_fold(0_usize, |total, (_, name)| {
        total
            .checked_add(name.len())
            .ok_or(AnalysisError::ArithmeticOverflow("hierarchy path"))
    })?;
    let separator_count = lineage.len().saturating_sub(1);
    let capacity = names_len
        .checked_add(separator_count)
        .ok_or(AnalysisError::ArithmeticOverflow("hierarchy path"))?;
    let mut path = reserve_string(capacity, "hierarchy path", budget)?;
    for (index, (_, name)) in lineage.iter().rev().enumerate() {
        if index != 0 {
            path.push('/');
        }
        path.push_str(name);
    }
    Ok(path)
}

fn collect_string_set(
    values: BTreeSet<String>,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<String>, AnalysisError> {
    let mut collected = reserve_retained_vec(values.len(), resource, budget)?;
    collected.extend(values);
    Ok(collected)
}

fn charge_hierarchy_set_entry(budget: &mut AssetLoadBudget) -> Result<(), AnalysisError> {
    let bytes = hierarchy_btree_entry_bytes()?;
    budget.check_entries(1)?;
    budget.check_members(1)?;
    budget.check_bytes(bytes)?;
    budget.consume_entries(1)?;
    budget.consume_members(1)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn hierarchy_btree_entry_bytes() -> Result<u64, AnalysisError> {
    let pointer_overhead =
        size_of::<usize>()
            .checked_mul(2)
            .ok_or(AnalysisError::ArithmeticOverflow(
                "hierarchy BTree pointer overhead",
            ))?;
    let slot_bytes = size_of::<String>().checked_add(pointer_overhead).ok_or(
        AnalysisError::ArithmeticOverflow("hierarchy BTree slot size"),
    )?;
    let metadata_bytes = size_of::<usize>()
        .checked_mul(HIERARCHY_BTREE_NODE_METADATA_WORDS)
        .ok_or(AnalysisError::ArithmeticOverflow(
            "hierarchy BTree metadata size",
        ))?;
    let node_bytes = slot_bytes
        .checked_mul(HIERARCHY_BTREE_MAX_NODE_SLOTS)
        .and_then(|bytes| bytes.checked_add(metadata_bytes))
        .ok_or(AnalysisError::ArithmeticOverflow(
            "hierarchy BTree node size",
        ))?;
    usize_to_u64(node_bytes)
}

fn clone_diagnostic(
    diagnostic: &Diagnostic,
    budget: &mut AssetLoadBudget,
) -> Result<Diagnostic, AnalysisError> {
    let code = clone_string(diagnostic.code(), "reference diagnostic code", budget)?;
    let message = clone_string(diagnostic.message(), "reference diagnostic message", budget)?;
    let address = diagnostic
        .address()
        .map(|address| clone_object_address(address, "reference diagnostic object", budget))
        .transpose()?;
    let field_path = diagnostic
        .field_path()
        .map(|path| clone_field_path(path, "reference diagnostic field path", budget))
        .transpose()?;
    let mut cloned = Diagnostic::new(diagnostic.severity(), code, message)?;
    if let Some(address) = address {
        cloned = cloned.at_address(address);
    }
    if let Some(field_path) = field_path {
        cloned = cloned.at_field(field_path);
    }
    Ok(cloned)
}

fn clone_field_path(
    path: &FieldPath,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<FieldPath, AnalysisError> {
    let mut segments = reserve_retained_vec(path.segments().len(), resource, budget)?;
    for segment in path.segments() {
        segments.push(match segment {
            FieldPathSegment::Field(name) => {
                FieldPathSegment::Field(clone_string(name, resource, budget)?)
            }
            FieldPathSegment::Index(index) => FieldPathSegment::Index(*index),
        });
    }
    FieldPath::from_segments(segments)
        .map_err(|_| AnalysisError::Invariant("cloned field path became invalid"))
}

fn clone_object_address(
    address: &ObjectAddress,
    _resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectAddress, AnalysisError> {
    let bytes = address
        .retained_clone_bytes()
        .ok_or(AnalysisError::ArithmeticOverflow("object address clone"))?;
    charge_foreign_clone(address.source_locator().members().len(), bytes, budget)?;
    Ok(address.clone())
}

fn clone_source_locator(
    locator: &SourceLocator,
    _resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<SourceLocator, AnalysisError> {
    let bytes = locator
        .retained_clone_bytes()
        .ok_or(AnalysisError::ArithmeticOverflow("source locator clone"))?;
    charge_foreign_clone(locator.members().len(), bytes, budget)?;
    Ok(locator.clone())
}

fn charge_foreign_clone(
    member_count: usize,
    bytes: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), AnalysisError> {
    let members = usize_to_u64(member_count)?;
    let bytes = usize_to_u64(bytes)?;
    budget.check_entries(members)?;
    budget.check_members(members)?;
    budget.check_bytes(bytes)?;
    budget.consume_entries(members)?;
    budget.consume_members(members)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn encode_guid(
    guid: [u8; 16],
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, AnalysisError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = reserve_string(32, resource, budget)?;
    for byte in guid {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn clone_ascii_lowercase(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, AnalysisError> {
    let mut cloned = reserve_string(value.len(), resource, budget)?;
    for character in value.chars() {
        cloned.push(character.to_ascii_lowercase());
    }
    Ok(cloned)
}

fn clone_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, AnalysisError> {
    let mut cloned = reserve_string(value.len(), resource, budget)?;
    cloned.push_str(value);
    Ok(cloned)
}

fn reserve_string(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, AnalysisError> {
    let bytes = usize_to_u64(capacity)?;
    budget.check_bytes(bytes)?;
    let mut value = String::new();
    value
        .try_reserve_exact(capacity)
        .map_err(|source| AnalysisError::Allocation {
            resource,
            requested: capacity,
            source,
        })?;
    budget.consume_bytes(bytes)?;
    Ok(value)
}

fn reserve_retained_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, AnalysisError> {
    let count = usize_to_u64(capacity)?;
    let requested =
        size_of::<T>()
            .checked_mul(capacity)
            .ok_or(AnalysisError::ArithmeticOverflow(
                "retained analysis vector",
            ))?;
    let bytes = usize_to_u64(requested)?;
    budget.check_entries(count)?;
    budget.check_members(count)?;
    budget.check_bytes(bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| AnalysisError::Allocation {
            resource,
            requested,
            source,
        })?;
    budget.consume_entries(count)?;
    budget.consume_members(count)?;
    budget.consume_bytes(bytes)?;
    Ok(values)
}

fn reserve_entry_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, AnalysisError> {
    let count = usize_to_u64(capacity)?;
    let requested =
        size_of::<T>()
            .checked_mul(capacity)
            .ok_or(AnalysisError::ArithmeticOverflow(
                "temporary analysis vector",
            ))?;
    let bytes = usize_to_u64(requested)?;
    budget.check_entries(count)?;
    budget.check_bytes(bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| AnalysisError::Allocation {
            resource,
            requested,
            source,
        })?;
    budget.consume_entries(count)?;
    budget.consume_bytes(bytes)?;
    Ok(values)
}

fn charge_locator(
    source: &WorkspaceSource,
    budget: &mut AssetLoadBudget,
) -> Result<(), AnalysisError> {
    charge_source_locator(source.locator(), budget)
}

fn charge_source_locator(
    locator: &SourceLocator,
    budget: &mut AssetLoadBudget,
) -> Result<(), AnalysisError> {
    let retained = locator
        .retained_clone_bytes()
        .ok_or(AnalysisError::ArithmeticOverflow("source locator"))?;
    budget.consume_bytes(usize_to_u64(retained)?)?;
    Ok(())
}

fn charge_address(
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<(), AnalysisError> {
    let retained = address
        .retained_clone_bytes()
        .ok_or(AnalysisError::ArithmeticOverflow("object address"))?;
    budget.consume_bytes(usize_to_u64(retained)?)?;
    Ok(())
}

fn charge_string(value: &str, budget: &mut AssetLoadBudget) -> Result<(), AnalysisError> {
    budget.consume_bytes(usize_to_u64(value.len())?)?;
    Ok(())
}

fn charge_entry<T>(budget: &mut AssetLoadBudget) -> Result<(), AnalysisError> {
    budget.consume_entries(1)?;
    budget.consume_bytes(usize_to_u64(size_of::<T>())?)?;
    Ok(())
}

fn charge_entries<T>(count: usize, budget: &mut AssetLoadBudget) -> Result<(), AnalysisError> {
    let entries = usize_to_u64(count)?;
    let bytes = size_of::<T>()
        .checked_mul(count)
        .ok_or(AnalysisError::ArithmeticOverflow("analysis entries"))?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(usize_to_u64(bytes)?)?;
    Ok(())
}

fn push_fallible<T>(
    values: &mut Vec<T>,
    value: T,
    resource: &'static str,
) -> Result<(), AnalysisError> {
    values
        .try_reserve(1)
        .map_err(|source| AnalysisError::Allocation {
            resource,
            requested: size_of::<T>(),
            source,
        })?;
    values.push(value);
    Ok(())
}

fn diagnostic(
    severity: DiagnosticSeverity,
    code: &'static str,
    message: impl Into<String>,
) -> Result<Diagnostic, AnalysisError> {
    Diagnostic::new(severity, code, message).map_err(Into::into)
}

fn bounded_error_message(error: &impl fmt::Display) -> String {
    const MAX_BYTES: usize = 4 * 1024;
    let value = error.to_string();
    if value.len() <= MAX_BYTES {
        return value;
    }
    let mut end = MAX_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn usize_to_u64(value: usize) -> Result<u64, AnalysisError> {
    u64::try_from(value).map_err(|_| AnalysisError::ArithmeticOverflow("usize to u64"))
}

fn u64_to_usize(value: u64) -> Result<usize, AnalysisError> {
    usize::try_from(value).map_err(|_| AnalysisError::ArithmeticOverflow("u64 to usize"))
}

#[derive(Debug)]
pub(crate) enum AnalysisError {
    Budget(BudgetError),
    Workspace(Box<WorkspaceError>),
    ReferenceGraph(Box<ReferenceGraphError>),
    Diagnostic(DiagnosticError),
    Contract(ContractError),
    ContextMismatch {
        view_workspace: WorkspaceId,
        view_revision: WorkspaceRevision,
        graph_workspace: WorkspaceId,
        graph_revision: WorkspaceRevision,
    },
    UnknownWorkspaceSource(SourceId),
    NotRootSource(SourceId),
    SourceHierarchyCycle(SourceId),
    CachedAnalysisHasNoWorkspaceSource,
    CachedAnalysisRootMismatch {
        cached: SourceId,
        requested: SourceId,
    },
    CachedAnalysisStale {
        root: SourceId,
        cached: Option<SourceFingerprint>,
        current: SourceFingerprint,
    },
    CachedGraphInputsIncomplete {
        root: SourceId,
    },
    ArithmeticOverflow(&'static str),
    Allocation {
        resource: &'static str,
        requested: usize,
        source: TryReserveError,
    },
    Invariant(&'static str),
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::Workspace(error) => fmt::Display::fmt(error, formatter),
            Self::ReferenceGraph(error) => fmt::Display::fmt(error, formatter),
            Self::Diagnostic(error) => fmt::Display::fmt(error, formatter),
            Self::Contract(error) => fmt::Display::fmt(error, formatter),
            Self::ContextMismatch {
                view_workspace,
                view_revision,
                graph_workspace,
                graph_revision,
            } => write!(
                formatter,
                "workspace view {view_workspace:?}@{view_revision} does not match reference graph {graph_workspace:?}@{graph_revision}"
            ),
            Self::UnknownWorkspaceSource(source) => {
                write!(
                    formatter,
                    "workspace analysis source is unknown: {source:?}"
                )
            }
            Self::NotRootSource(source) => {
                write!(
                    formatter,
                    "workspace analysis source is not a root: {source:?}"
                )
            }
            Self::SourceHierarchyCycle(source) => {
                write!(
                    formatter,
                    "workspace source hierarchy contains a cycle at {source:?}"
                )
            }
            Self::CachedAnalysisHasNoWorkspaceSource => {
                formatter.write_str("cached analysis has no workspace root source")
            }
            Self::CachedAnalysisRootMismatch { cached, requested } => write!(
                formatter,
                "cached analysis root {cached:?} does not match requested root {requested:?}"
            ),
            Self::CachedAnalysisStale {
                root,
                cached,
                current,
            } => write!(
                formatter,
                "cached analysis fingerprint {cached:?} is stale for root {root:?}; current fingerprint is {current}"
            ),
            Self::CachedGraphInputsIncomplete { root } => write!(
                formatter,
                "cached analysis for root {root:?} lacks complete graph refresh inputs"
            ),
            Self::ArithmeticOverflow(resource) => {
                write!(formatter, "analysis arithmetic overflow for {resource}")
            }
            Self::Allocation {
                resource,
                requested,
                ..
            } => write!(
                formatter,
                "failed to reserve {requested} bytes for {resource}"
            ),
            Self::Invariant(message) => {
                write!(formatter, "analysis invariant failed: {message}")
            }
        }
    }
}

impl StdError for AnalysisError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::Workspace(error) => Some(error.as_ref()),
            Self::ReferenceGraph(error) => Some(error.as_ref()),
            Self::Diagnostic(error) => Some(error),
            Self::Contract(error) => Some(error),
            Self::Allocation { source, .. } => Some(source),
            Self::ContextMismatch { .. }
            | Self::UnknownWorkspaceSource(_)
            | Self::NotRootSource(_)
            | Self::SourceHierarchyCycle(_)
            | Self::CachedAnalysisHasNoWorkspaceSource
            | Self::CachedAnalysisRootMismatch { .. }
            | Self::CachedAnalysisStale { .. }
            | Self::CachedGraphInputsIncomplete { .. }
            | Self::ArithmeticOverflow(_)
            | Self::Invariant(_) => None,
        }
    }
}

impl From<BudgetError> for AnalysisError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<WorkspaceError> for AnalysisError {
    fn from(error: WorkspaceError) -> Self {
        Self::Workspace(Box::new(error))
    }
}

impl From<ReferenceGraphError> for AnalysisError {
    fn from(error: ReferenceGraphError) -> Self {
        Self::ReferenceGraph(Box::new(error))
    }
}

impl From<DiagnosticError> for AnalysisError {
    fn from(error: DiagnosticError) -> Self {
        Self::Diagnostic(error)
    }
}

impl From<ContractError> for AnalysisError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tempfile::tempdir;
    use unity_asset::reference::ReferenceGraphBuildOptions;
    use unity_asset::workspace::{AssetWorkspace, SourceOpenRequest};
    use unity_asset::{AssetLoadLimits, BudgetedSourceBytes, DigestV1, SourceAlias, YamlFileId};

    use super::*;
    use crate::analysis::protocol_object_file_id;
    use crate::scan::{FileHint, SourceHints};

    #[test]
    fn protocol_file_ids_never_alias_yaml_document_ordinals() {
        let locator = SourceLocator::path("Assets/Scene.unity").unwrap();
        let binary = ObjectAddress::binary_direct(locator.clone(), -7).unwrap();
        let yaml = ObjectAddress::yaml(locator.clone(), YamlFileId::new(7).unwrap()).unwrap();
        let unanchored = ObjectAddress::yaml_document(locator, 7).unwrap();

        assert_eq!(protocol_object_file_id(&binary), Some(-7));
        assert_eq!(protocol_object_file_id(&yaml), Some(7));
        assert_eq!(protocol_object_file_id(&unanchored), None);
    }

    #[test]
    fn normalized_terms_use_exact_requested_layout_budget() {
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 6,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert_eq!(budgeted_terms("Simple", &mut exact).unwrap(), "simple");
        assert_eq!(exact.usage().bytes, 6);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 5,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            budgeted_terms("Simple", &mut short),
            Err(AnalysisError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 5,
                requested: 6,
            }))
        ));
        assert_eq!(short.usage().bytes, 0);
    }

    #[test]
    fn allocation_error_preserves_the_typed_reserve_source() {
        let source = String::new().try_reserve(usize::MAX).unwrap_err();
        let error = AnalysisError::Allocation {
            resource: "test allocation",
            requested: usize::MAX,
            source,
        };

        assert_eq!(
            error.to_string(),
            format!("failed to reserve {} bytes for test allocation", usize::MAX)
        );
        assert!(
            StdError::source(&error)
                .and_then(|source| source.downcast_ref::<TryReserveError>())
                .is_some()
        );
    }

    #[test]
    fn long_invalid_guid_is_rejected_without_an_input_sized_allocation() {
        let input = "f".repeat(1_000_000);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert_eq!(normalize_guid(&input, &mut budget).unwrap(), None);
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn valid_guid_reserves_its_fixed_output_before_writing() {
        let input = "00112233-4455-6677-8899-aabbccddeeff";
        let expected = "00112233445566778899aabbccddeeff";
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 32,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert_eq!(
            normalize_guid(input, &mut exact).unwrap().as_deref(),
            Some(expected)
        );
        assert_eq!(exact.usage().bytes, 32);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 31,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            normalize_guid(input, &mut short),
            Err(AnalysisError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 31,
                requested: 32,
            }))
        ));
        assert_eq!(short.usage().bytes, 0);
    }

    #[test]
    fn hierarchy_path_uses_the_exact_checked_layout() {
        let root = ObjectAddress::yaml(
            SourceLocator::path("Assets/Scene.unity").unwrap(),
            "1".parse().unwrap(),
        )
        .unwrap();
        let child = ObjectAddress::yaml(
            SourceLocator::path("Assets/Scene.unity").unwrap(),
            "2".parse().unwrap(),
        )
        .unwrap();
        let lineage = [(&child, "Child"), (&root, "Root")];
        let expected = "Root/Child";
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(expected.len()).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert_eq!(hierarchy_path(&lineage, &mut exact).unwrap(), expected);
        assert_eq!(exact.usage().bytes, u64::try_from(expected.len()).unwrap());

        let mut one_over = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(expected.len() - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            hierarchy_path(&lineage, &mut one_over),
            Err(AnalysisError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 9,
                requested: 10,
            }))
        ));
        assert_eq!(one_over.usage().bytes, 0);
    }

    #[test]
    fn nested_reference_backings_are_preflighted_before_deep_clones() {
        let locator =
            SourceLocator::archive_member("Assets/References", "nested/target.asset").unwrap();
        let first = ObjectAddress::yaml(locator.clone(), "100".parse().unwrap()).unwrap();
        let second = ObjectAddress::yaml(locator, "200".parse().unwrap()).unwrap();
        let candidates = vec![first.clone(), second.clone(), first.clone()];
        let candidate_backing = size_of::<ObjectAddress>() * candidates.len();
        let mut candidate_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(candidate_backing - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert!(matches!(
            clone_ambiguous_candidates(&candidates, &mut candidate_short),
            Err(AnalysisError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit + 1 == requested
        ));
        assert_eq!(candidate_short.usage().bytes, 0);

        let diagnostic = Diagnostic::new(
            DiagnosticSeverity::Warning,
            "REFERENCE_TEST",
            "nested reference diagnostic",
        )
        .unwrap()
        .at_address(first.clone())
        .at_field(FieldPath::root().push_field("m_Target").unwrap());
        let resolution = ReferenceResolution::Invalid {
            diagnostic: diagnostic.clone(),
        };
        let diagnostic_backing = size_of::<Diagnostic>() * 2;
        let mut diagnostic_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(diagnostic_backing - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert!(matches!(
            clone_reference_diagnostics(
                std::slice::from_ref(&diagnostic),
                &resolution,
                &mut diagnostic_short,
            ),
            Err(AnalysisError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit + 1 == requested
        ));
        assert_eq!(diagnostic_short.usage().bytes, 0);

        let raw_guid = "AA-BB-CC-DD-EE-FF-00-11-22-33-44-55-66-77-88-99";
        let raw_target = RawReferenceTarget::Yaml {
            file_id: Some(7),
            guid: Some(ReferenceGuid::Invalid(raw_guid.to_owned())),
            type_id: Some(3),
        };
        let mut raw_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(raw_guid.len() - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            project_raw_reference(&raw_target, &mut raw_short),
            Err(AnalysisError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit + 1 == requested
        ));
        assert_eq!(raw_short.usage().bytes, 0);

        let raw = project_raw_reference(&raw_target, &mut AssetLoadBudget::default()).unwrap();
        let projected_resolution = ReferenceResolutionProjection::Ambiguous {
            candidates: clone_ambiguous_candidates(&candidates, &mut AssetLoadBudget::default())
                .unwrap(),
        };
        let dependency_backing = size_of::<ReferenceDependencyKey>() * 3;
        let mut dependency_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(dependency_backing - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            dependency_keys(&raw, &projected_resolution, &mut dependency_short),
            Err(AnalysisError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit + 1 == requested
        ));
        assert_eq!(dependency_short.usage().bytes, 0);

        let keys =
            dependency_keys(&raw, &projected_resolution, &mut AssetLoadBudget::default()).unwrap();

        assert_eq!(keys.len(), 3);
        assert!(matches!(
            &keys[0],
            ReferenceDependencyKey::Guid { guid, file_id: Some(7) }
                if guid == "aa-bb-cc-dd-ee-ff-00-11-22-33-44-55-66-77-88-99"
        ));
    }

    fn memory_source(
        relative_path: &str,
        kind: SearchKind,
        bytes: Option<Arc<[u8]>>,
        budget: &mut AssetLoadBudget,
    ) -> ReadSource {
        let length = bytes
            .as_ref()
            .map_or(128, |bytes| u64::try_from(bytes.len()).unwrap());
        let byte_digest = bytes.as_deref().map_or_else(
            || DigestV1::hash_bytes(b"not-retained"),
            DigestV1::hash_bytes,
        );
        let bytes = bytes.map(|bytes| BudgetedSourceBytes::from_arc(bytes, budget).unwrap());
        ReadSource {
            rel_path: relative_path.to_owned(),
            abs_path: PathBuf::from("this/path/must/not/be/opened"),
            name: relative_path
                .rsplit('/')
                .next()
                .unwrap_or(relative_path)
                .to_owned(),
            kind,
            guid: None,
            bytes,
            meta_bytes: None,
            length,
            content_identity: byte_digest,
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
    fn missing_payload_keeps_identity_and_reports_incomplete_coverage() {
        let mut budget = AssetLoadBudget::default();
        let source = memory_source("Assets/Huge.bin", SearchKind::File, None, &mut budget);

        let output = AssetAnalyzer::default()
            .analyze(&source, None, &mut budget)
            .unwrap();

        assert_eq!(output.analysis.source.relative_path, "Assets/Huge.bin");
        assert_eq!(output.analysis.search.display_name, "Huge.bin");
        assert!(!output.analysis.complete);
        assert_eq!(
            output.analysis.truncations,
            vec![AnalysisTruncation::new(
                AnalysisTruncationKind::PayloadUnavailable,
                None,
                128,
            )]
        );
        assert_eq!(output.metrics.source_opens, 0);
        assert_eq!(output.metrics.source_bytes_read, 0);
    }

    #[test]
    fn script_analysis_is_deterministic_and_uses_only_retained_bytes() {
        let bytes: Arc<[u8]> = Arc::from(
            b"namespace Example.Game;\npublic sealed class PlayerController {}\n".as_slice(),
        );
        let analyzer = AssetAnalyzer::default();

        let mut first_budget = AssetLoadBudget::default();
        let first_source = memory_source(
            "Assets/Scripts/PlayerController.cs",
            SearchKind::Script,
            Some(Arc::clone(&bytes)),
            &mut first_budget,
        );
        let first = analyzer
            .analyze(&first_source, None, &mut first_budget)
            .unwrap();
        let mut second_budget = AssetLoadBudget::default();
        let second_source = memory_source(
            "Assets/Scripts/PlayerController.cs",
            SearchKind::Script,
            Some(bytes),
            &mut second_budget,
        );
        let second = analyzer
            .analyze(&second_source, None, &mut second_budget)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first.analysis.search.script_symbols,
            vec!["Example.Game".to_owned(), "PlayerController".to_owned()]
        );
        assert!(
            first
                .analysis
                .search
                .content_terms
                .contains("player controller")
        );
        assert_eq!(first.metrics.source_opens, 0);
        assert_eq!(first.metrics.source_bytes_read, 0);
    }

    #[test]
    fn analysis_rejects_a_payload_from_another_budget_domain_before_accounting() {
        let mut source_budget = AssetLoadBudget::default();
        let source = memory_source(
            "Assets/Scripts/PlayerController.cs",
            SearchKind::Script,
            Some(Arc::from(
                b"public sealed class PlayerController {}\n".as_slice(),
            )),
            &mut source_budget,
        );
        let mut analysis_budget = AssetLoadBudget::default();

        let error = AssetAnalyzer::default()
            .analyze(&source, None, &mut analysis_budget)
            .unwrap_err();

        assert!(matches!(
            error,
            AnalysisError::Budget(BudgetError::DomainMismatch {
                resource: "source bytes"
            })
        ));
        assert_eq!(analysis_budget.usage(), Default::default());
    }

    #[test]
    fn workspace_analysis_uses_the_supplied_graph_and_preserves_negative_ids() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("Negative.prefab");
        let payload: Arc<[u8]> = Arc::from(
            br#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &-42
MonoBehaviour:
  m_ObjectHideFlags: 0
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 0}
  m_Enabled: 1
  m_EditorHideFlags: 0
  m_Script: {fileID: -11500000, guid: 00112233445566778899aabbccddeeff, type: 3}
  m_Name: NegativeIds
  m_EditorClassIdentifier:
"#
            .as_slice(),
        );
        std::fs::write(&path, payload.as_ref()).unwrap();

        let mut workspace = AssetWorkspace::new().unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let root = workspace
            .load_source_bytes(
                SourceOpenRequest::new(&path, SourceAlias::new("Assets/Negative.prefab").unwrap())
                    .with_kind_hint(SourceKind::Yaml),
                Arc::clone(&payload),
                &mut load_budget,
            )
            .unwrap();
        let snapshot = workspace.snapshot();
        let graph = ReferenceGraph::build(
            &snapshot,
            ReferenceGraphBuildOptions::unbounded(),
            &mut load_budget,
        )
        .unwrap();
        let context = WorkspaceAnalysisContext::build(&snapshot, &graph, &mut load_budget).unwrap();

        std::fs::remove_file(&path).unwrap();
        let mut analysis_budget = AssetLoadBudget::default();
        let mut source = memory_source(
            "Assets/Negative.prefab",
            SearchKind::Prefab,
            Some(Arc::clone(&payload)),
            &mut analysis_budget,
        );
        source.abs_path = directory.path().join("definitely-missing.prefab");
        source.bytes = None;
        let analyzer = AssetAnalyzer::default();
        let output = analyzer
            .analyze(
                &source,
                Some(context.asset(root).unwrap()),
                &mut analysis_budget,
            )
            .unwrap();

        let script_reference = output
            .analysis
            .references
            .iter()
            .find(|reference| path_contains_field(reference.field_path.segments(), "m_Script"))
            .expect("the supplied graph contributes the m_Script fact");
        assert_eq!(script_reference.protocol_file_id(), Some(-42));
        assert_eq!(script_reference.source_class_id, Some(114));
        assert!(matches!(
            &script_reference.raw_target,
            RawReferenceProjection::Yaml {
                file_id: Some(-11_500_000),
                ..
            }
        ));
        assert_eq!(
            output.analysis.search.referenced_script_guids,
            vec!["00112233445566778899aabbccddeeff".to_owned()]
        );
        assert_eq!(output.analysis.references.len(), graph.facts().len());
        assert!(output.analysis.graph_inputs.complete);
        assert_eq!(output.metrics.source_opens, 0);
        assert_eq!(output.metrics.source_bytes_read, 0);

        let mut refresh_budget = AssetLoadBudget::default();
        let refreshed = analyzer
            .refresh_graph_facts(
                &output.analysis,
                context.asset_for_analysis(&output.analysis).unwrap(),
                &mut refresh_budget,
            )
            .unwrap();
        assert_eq!(refreshed.analysis.references, output.analysis.references);
        assert_eq!(
            refreshed.analysis.container_entries,
            output.analysis.container_entries
        );
        assert_eq!(refreshed.analysis.search, output.analysis.search);
        assert_eq!(refreshed.metrics.unity_values_visited, 0);
        assert_eq!(refreshed.metrics.source_opens, 0);
        assert_eq!(refreshed.metrics.source_bytes_read, 0);
    }

    #[test]
    fn binary_projection_keeps_negative_file_and_path_ids_signed() {
        let raw = RawReferenceTarget::Binary {
            file_id: -3,
            path_id: -9,
            external: None,
        };
        let mut budget = AssetLoadBudget::default();
        assert_eq!(
            project_raw_reference(&raw, &mut budget).unwrap(),
            RawReferenceProjection::Binary {
                file_id: -3,
                path_id: -9,
                external: None,
            }
        );

        let address =
            ObjectAddress::binary_direct(SourceLocator::path("Assets/data.assets").unwrap(), -9)
                .unwrap();
        assert_eq!(protocol_object_file_id(&address), Some(-9));
    }
}
