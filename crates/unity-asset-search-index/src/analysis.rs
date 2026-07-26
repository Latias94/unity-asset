use serde::{Deserialize, Serialize};
use unity_asset_core::{
    Diagnostic, DigestV1, FieldPath, ObjectAddress, SourceFingerprint, SourceId, SourceLocator,
    TransactionId, WorkspaceId, WorkspaceRevision,
};
use unity_asset_search_core::SearchKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetAnalysisBatch {
    pub(crate) workspace: WorkspaceId,
    pub(crate) revision: WorkspaceRevision,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) transactions: Vec<TransactionId>,
    pub(crate) assets: Vec<AssetAnalysis>,
    pub(crate) metrics: AnalysisMetrics,
}

impl AssetAnalysisBatch {
    pub(crate) fn new(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        mut transactions: Vec<TransactionId>,
        mut assets: Vec<AssetAnalysis>,
        metrics: AnalysisMetrics,
    ) -> Self {
        transactions.sort_unstable();
        transactions.dedup();
        assets.sort_by(|left, right| left.source.relative_path.cmp(&right.source.relative_path));
        Self {
            workspace,
            revision,
            transactions,
            assets,
            metrics,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnalysisMetrics {
    pub(crate) assets_visited: u64,
    pub(crate) assets_analyzed: u64,
    pub(crate) source_opens: u64,
    pub(crate) source_bytes_read: u64,
    pub(crate) text_sources: u64,
    pub(crate) text_bytes_scanned: u64,
    pub(crate) yaml_documents: u64,
    pub(crate) binary_objects: u64,
    pub(crate) unity_values_visited: u64,
    pub(crate) references_emitted: u64,
    pub(crate) container_entries_emitted: u64,
    pub(crate) truncations_emitted: u64,
    pub(crate) diagnostics_emitted: u64,
}

impl AnalysisMetrics {
    pub(crate) fn merge(&mut self, other: &Self) {
        self.assets_visited = self.assets_visited.saturating_add(other.assets_visited);
        self.assets_analyzed = self.assets_analyzed.saturating_add(other.assets_analyzed);
        self.source_opens = self.source_opens.saturating_add(other.source_opens);
        self.source_bytes_read = self
            .source_bytes_read
            .saturating_add(other.source_bytes_read);
        self.text_sources = self.text_sources.saturating_add(other.text_sources);
        self.text_bytes_scanned = self
            .text_bytes_scanned
            .saturating_add(other.text_bytes_scanned);
        self.yaml_documents = self.yaml_documents.saturating_add(other.yaml_documents);
        self.binary_objects = self.binary_objects.saturating_add(other.binary_objects);
        self.unity_values_visited = self
            .unity_values_visited
            .saturating_add(other.unity_values_visited);
        self.references_emitted = self
            .references_emitted
            .saturating_add(other.references_emitted);
        self.container_entries_emitted = self
            .container_entries_emitted
            .saturating_add(other.container_entries_emitted);
        self.truncations_emitted = self
            .truncations_emitted
            .saturating_add(other.truncations_emitted);
        self.diagnostics_emitted = self
            .diagnostics_emitted
            .saturating_add(other.diagnostics_emitted);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetAnalysis {
    pub(crate) source: AnalyzedSource,
    pub(crate) search: SearchFacts,
    #[serde(default, skip_serializing_if = "WorkspaceGraphInputs::is_empty")]
    pub(crate) graph_inputs: WorkspaceGraphInputs,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) references: Vec<ReferenceProjectionFact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) container_entries: Vec<ContainerEntryFact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) truncations: Vec<AnalysisTruncation>,
    pub(crate) complete: bool,
}

impl AssetAnalysis {
    #[cfg(test)]
    pub(crate) fn new(
        source: AnalyzedSource,
        search: SearchFacts,
        references: Vec<ReferenceProjectionFact>,
        container_entries: Vec<ContainerEntryFact>,
        diagnostics: Vec<Diagnostic>,
        complete: bool,
    ) -> Self {
        Self::with_truncations(
            source,
            search,
            references,
            container_entries,
            diagnostics,
            Vec::new(),
            complete,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_truncations(
        source: AnalyzedSource,
        search: SearchFacts,
        references: Vec<ReferenceProjectionFact>,
        container_entries: Vec<ContainerEntryFact>,
        diagnostics: Vec<Diagnostic>,
        truncations: Vec<AnalysisTruncation>,
        complete: bool,
    ) -> Self {
        Self::with_graph_inputs(
            source,
            search,
            WorkspaceGraphInputs::default(),
            references,
            container_entries,
            diagnostics,
            truncations,
            complete,
        )
    }

    pub(crate) fn with_graph_inputs(
        source: AnalyzedSource,
        mut search: SearchFacts,
        mut graph_inputs: WorkspaceGraphInputs,
        mut references: Vec<ReferenceProjectionFact>,
        mut container_entries: Vec<ContainerEntryFact>,
        mut diagnostics: Vec<Diagnostic>,
        mut truncations: Vec<AnalysisTruncation>,
        complete: bool,
    ) -> Self {
        search.normalize();
        graph_inputs.normalize();
        for reference in &mut references {
            reference.normalize();
        }
        references.sort();
        references.dedup();
        container_entries.sort();
        container_entries.dedup();
        diagnostics.sort();
        diagnostics.dedup();
        truncations.sort();
        truncations.dedup();
        let complete = complete && truncations.is_empty();
        Self {
            source,
            search,
            graph_inputs,
            references,
            container_entries,
            diagnostics,
            truncations,
            complete,
        }
    }

    pub(crate) fn record_incomplete(
        &mut self,
        diagnostic: Diagnostic,
        truncation: AnalysisTruncation,
    ) {
        self.diagnostics.push(diagnostic);
        self.diagnostics.sort();
        self.diagnostics.dedup();
        self.truncations.push(truncation);
        self.truncations.sort();
        self.truncations.dedup();
        self.complete = false;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceGraphInputs {
    #[serde(default)]
    pub(crate) complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) objects: Vec<WorkspaceObjectFact>,
}

impl WorkspaceGraphInputs {
    pub(crate) fn new(objects: Vec<WorkspaceObjectFact>, complete: bool) -> Self {
        let mut inputs = Self { complete, objects };
        inputs.normalize();
        inputs
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.complete && self.objects.is_empty()
    }

    fn normalize(&mut self) {
        self.objects.sort();
        self.objects
            .dedup_by(|left, right| left.address == right.address);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceObjectFact {
    pub(crate) address: ObjectAddress,
    pub(crate) class_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnalysisTruncationKind {
    PayloadUnavailable,
    SourceAssetBytes,
    SourceMetaBytes,
    TextBytes,
    WorkspaceObjects,
    UnityValues,
    ContentTerms,
    HierarchyPaths,
    HierarchyDepth,
    ScriptSymbols,
    ReferencedScriptGuids,
    ContainerEntries,
    ReferenceFacts,
    ReferenceGraphNodes,
    ReferenceGraphFacts,
    GraphRefreshInputs,
    WorkspaceParseFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnalysisTruncation {
    pub(crate) kind: AnalysisTruncationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<u64>,
    pub(crate) observed_at_least: u64,
}

impl AnalysisTruncation {
    pub(crate) const fn new(
        kind: AnalysisTruncationKind,
        limit: Option<u64>,
        observed_at_least: u64,
    ) -> Self {
        Self {
            kind,
            limit,
            observed_at_least,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnalyzedSource {
    pub(crate) relative_path: String,
    pub(crate) content_digest: DigestV1,
    pub(crate) length: u64,
    pub(crate) search_kind: SearchKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_source: Option<SourceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_fingerprint: Option<SourceFingerprint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) locator: Option<SourceLocator>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchFacts {
    pub(crate) display_name: String,
    pub(crate) path_terms: String,
    pub(crate) name_terms: String,
    pub(crate) content_terms: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) hierarchy_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) script_symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) referenced_script_guids: Vec<String>,
}

impl SearchFacts {
    fn normalize(&mut self) {
        sort_strings(&mut self.hierarchy_paths);
        sort_strings(&mut self.script_symbols);
        sort_strings(&mut self.referenced_script_guids);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContainerEntryFact {
    pub(crate) asset_path: String,
    pub(crate) file_id: i32,
    pub(crate) path_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReferenceProjectionFact {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_object: Option<ObjectAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_file_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_class_id: Option<i32>,
    pub(crate) field_path: FieldPath,
    pub(crate) raw_target: RawReferenceProjection,
    pub(crate) resolution: ReferenceResolutionProjection,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) dependency_keys: Vec<ReferenceDependencyKey>,
}

impl ReferenceProjectionFact {
    pub(crate) fn normalize(&mut self) {
        self.diagnostics.sort();
        self.diagnostics.dedup();
        self.dependency_keys.sort();
        self.dependency_keys.dedup();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub(crate) enum RawReferenceProjection {
    Binary {
        file_id: i32,
        path_id: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        external: Option<BinaryExternalProjection>,
    },
    Yaml {
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        guid: Option<GuidProjection>,
        #[serde(skip_serializing_if = "Option::is_none")]
        type_id: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BinaryExternalProjection {
    pub(crate) index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) guid: Option<[u8; 16]>,
    pub(crate) type_id: i32,
    pub(crate) path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub(crate) enum GuidProjection {
    Parsed([u8; 16]),
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ReferenceResolutionProjection {
    Null,
    Resolved {
        target: ObjectAddress,
    },
    Unloaded {
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<SourceLocator>,
    },
    Missing {
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<ObjectAddress>,
    },
    Ambiguous {
        candidates: Vec<ObjectAddress>,
    },
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ReferenceDependencyKey {
    Guid {
        guid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<i64>,
    },
    Object {
        address: ObjectAddress,
    },
    Source {
        locator: SourceLocator,
    },
}

fn sort_strings(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}
