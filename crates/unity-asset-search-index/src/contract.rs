use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use unity_asset_core::{Diagnostic, ObjectAddress};
use unity_asset_search_core::{
    FuzzyWorkUsage, HighlightRange, MatchCount, MatchExplanation, MatchKind, RankingSignals,
    SearchDiagnostic,
};

use crate::generation::{
    GenerationStamp, GenerationStatus, SEARCH_GENERATION_CONTRACT_VERSION, SearchGenerationId,
};

/// Maximum number of diagnostics retained in one references response.
pub const MAX_REFERENCE_RESPONSE_DIAGNOSTICS: usize = 128;

/// Maximum serialized JSON bytes retained by the diagnostics array in one references response.
pub const MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub rank: usize,
    pub guid: Option<String>,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub stable_id: String,
    pub location: Location,
    pub ranking_signals: RankingSignals,
    pub match_kind: MatchKind,
    pub explanation: MatchExplanation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_hierarchy_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_script_symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlight_path_ranges: Vec<HighlightRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlight_name_ranges: Vec<HighlightRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_id: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub contract_version: u16,
    pub generation: GenerationStamp,
    pub query: String,
    pub took_ms: u128,
    pub match_count: MatchCount,
    pub returned_hits: usize,
    pub request_limit_truncated: bool,
    pub fuzzy_work: FuzzyWorkUsage,
    pub hits: Vec<SearchHit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<SearchDiagnostic>,
    #[serde(default)]
    pub fallback_used: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceHit {
    pub source_path: String,
    pub source_kind: String,
    pub stable_id: String,
    pub location: Location,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<ReferenceContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<ReferenceObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_file_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_class_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hierarchy_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_column: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceObject {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_file_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_class_id: Option<i32>,
    pub stable_id: String,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hierarchy_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencesResponse {
    pub contract_version: u16,
    pub generation: GenerationStamp,
    pub request: ReferenceRequest,
    pub took_ms: u128,
    pub coverage: ReferenceCoverage,
    pub hits: Vec<ReferenceHit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub diagnostic_coverage: ReferenceDiagnosticCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReferenceDiagnosticCoverage {
    /// Diagnostics retained in [`ReferencesResponse::diagnostics`].
    pub returned: usize,
    /// Whether diagnostics were omitted by a generation or response limit.
    pub truncated: bool,
    /// Exact diagnostics considered for this page when the generation diagnostic projection was not
    /// truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// JSON bytes in the diagnostics array, including its brackets and separators.
    pub serialized_bytes: usize,
    /// Maximum diagnostics this response is allowed to retain.
    pub max_count: usize,
    /// Maximum JSON bytes this response is allowed to retain for diagnostics.
    pub max_serialized_bytes: usize,
}

impl Default for ReferenceDiagnosticCoverage {
    fn default() -> Self {
        Self {
            returned: 0,
            truncated: false,
            total: None,
            serialized_bytes: 2,
            max_count: MAX_REFERENCE_RESPONSE_DIAGNOSTICS,
            max_serialized_bytes: MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResponse {
    pub contract_version: u16,
    pub generation: GenerationStamp,
    pub prefix: String,
    pub took_ms: u128,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexProgress {
    pub operation: String,
    pub phase: String,
    pub phase_index: u32,
    pub phase_count: u32,
    pub phases: Vec<String>,
    pub processed: u64,
    pub total: u64,
    pub has_total: bool,
    pub started_unix_ms: u64,
    pub updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub contract_version: u16,
    pub generation: GenerationStatus,
    pub capabilities: SearchCapabilities,
    pub project_root: PathBuf,
    pub generation_root: PathBuf,
    pub scan_roots: Vec<PathBuf>,
    pub indexed_assets: u64,
    pub indexed_search_documents: u64,
    pub indexed_reference_facts: u64,
    pub incomplete_assets: u64,
    pub projection_truncations: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_build_duration_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_build_unix_ms: Option<u64>,
    pub indexing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<IndexProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReferenceSelector {
    Object {
        address: ObjectAddress,
    },
    Guid {
        guid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCursor {
    pub generation: SearchGenerationId,
    pub after_stable_id: String,
    /// Opaque identity of the normalized reference selector and direction.
    ///
    /// The optional wire representation lets legacy cursors deserialize so the query layer can
    /// reject them with `invalid_cursor` instead of collapsing the failure into malformed JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_binding: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceRequest {
    pub contract_version: u16,
    pub direction: ReferenceDirection,
    pub selector: ReferenceSelector,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ReferenceCursor>,
}

impl ReferenceRequest {
    #[must_use]
    pub fn incoming_object(address: ObjectAddress, limit: usize) -> Self {
        Self::new(
            ReferenceDirection::Incoming,
            ReferenceSelector::Object { address },
            limit,
        )
    }

    #[must_use]
    pub fn incoming_guid(guid: impl Into<String>, file_id: Option<i64>, limit: usize) -> Self {
        Self::guid(ReferenceDirection::Incoming, guid, file_id, limit)
    }

    #[must_use]
    pub fn outgoing_object(address: ObjectAddress, limit: usize) -> Self {
        Self::new(
            ReferenceDirection::Outgoing,
            ReferenceSelector::Object { address },
            limit,
        )
    }

    #[must_use]
    pub fn outgoing_guid(guid: impl Into<String>, file_id: Option<i64>, limit: usize) -> Self {
        Self::guid(ReferenceDirection::Outgoing, guid, file_id, limit)
    }

    #[must_use]
    pub fn with_cursor(mut self, cursor: ReferenceCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    fn guid(
        direction: ReferenceDirection,
        guid: impl Into<String>,
        file_id: Option<i64>,
        limit: usize,
    ) -> Self {
        Self::new(
            direction,
            ReferenceSelector::Guid {
                guid: guid.into(),
                file_id,
            },
            limit,
        )
    }

    fn new(direction: ReferenceDirection, selector: ReferenceSelector, limit: usize) -> Self {
        Self {
            contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
            direction,
            selector,
            limit,
            cursor: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCoverage {
    pub complete: bool,
    pub truncated: bool,
    pub returned: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<ReferenceCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    InvalidRequest,
    InvalidCursor,
    Unauthorized,
    ForbiddenListener,
    Busy,
    GenerationUnavailable,
    RevisionMismatch,
    IndexBuildFailed,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    pub contract_version: u16,
    pub code: ApiErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationStamp>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

impl ApiError {
    #[must_use]
    pub fn new(code: ApiErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
            code,
            message: message.into(),
            retryable,
            generation: None,
            details: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_generation(mut self, generation: GenerationStamp) -> Self {
        self.generation = Some(generation);
        self
    }

    #[must_use]
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCapabilities {
    pub contract_version: u16,
    pub search: bool,
    pub suggest: bool,
    pub incoming_references: bool,
    pub outgoing_references: bool,
    pub full_reindex: bool,
    pub changed_path_reindex: bool,
    pub change_set_reindex: bool,
    pub generation_barrier: bool,
}

impl SearchCapabilities {
    #[must_use]
    pub const fn current() -> Self {
        Self {
            contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
            search: true,
            suggest: true,
            incoming_references: true,
            outgoing_references: true,
            full_reindex: true,
            changed_path_reindex: true,
            change_set_reindex: true,
            generation_barrier: true,
        }
    }
}
