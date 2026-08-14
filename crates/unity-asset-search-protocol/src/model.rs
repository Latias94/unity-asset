use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use unity_asset_core::{
    Diagnostic, DigestV1, ObjectAddress, TransactionId, WorkspaceId, WorkspaceRevision,
};
use unity_asset_search_core::{
    CandidateField, FuzzyWorkUsage, HighlightRange, MatchCount, MatchCountRelation,
    MatchExplanation, MatchField, MatchKind, RankingSignals, RetrievalStage, SearchDiagnostic,
    TermExplanation,
};

use crate::operation::{BackgroundReindexOperation, validate_background_reindex_operations};
use crate::validation::{ContractValidationError, ValidateContract, ensure_revision};
use crate::{MAX_REFERENCE_RESULTS, QueryPolicyId};

pub const SEARCH_PROTOCOL_REVISION: u16 = 5;
pub const MAX_API_ERROR_JSON_BYTES: u64 = 224 * 1024;
pub const MAX_ERROR_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_PORTABLE_PATH_BYTES: usize = 32 * 1024;
pub const MAX_REINDEX_PUBLISH_WARNING_BYTES: usize = 4 * 1024;
pub const MAX_REINDEX_PUBLISH_WARNINGS: usize = 64;
pub const MAX_REINDEX_PUBLISH_WARNINGS_JSON_BYTES: u64 = 224 * 1024;
pub const MAX_REFERENCE_RESPONSE_DIAGNOSTICS: u32 = 128;
pub const MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES: u64 = 256 * 1024;
pub const MAX_SEARCH_DIAGNOSTICS_JSON_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_SEARCH_HITS_JSON_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_SEARCH_RESPONSE_JSON_BYTES: u64 = 15 * 1024 * 1024;
pub const MAX_SEARCH_RESPONSE_DIAGNOSTICS: usize = 4_096;
pub const MAX_STATUS_SCAN_ROOTS: usize = 64;
pub const MAX_STATUS_PATHS_JSON_BYTES: u64 = 224 * 1024;
pub const MAX_SUGGESTION_BYTES: usize = 32 * 1024;
pub const MAX_SUGGESTIONS_JSON_BYTES: u64 = 224 * 1024;
const REFERENCE_CURSOR_BINDING_DOMAIN: &[u8] = b"unity-asset:reference-query:cursor-binding:v2\0";
const REFERENCE_CURSOR_BINDING_PREFIX: &str = "reference-query-v2:";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortablePath(String);

impl PortablePath {
    pub fn new(value: impl Into<String>) -> Result<Self, PortablePathError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn validate(value: &str) -> Result<(), PortablePathError> {
        if value.is_empty() {
            return Err(PortablePathError::Empty);
        }
        if value.len() > MAX_PORTABLE_PATH_BYTES {
            return Err(PortablePathError::TooLong {
                actual: value.len(),
                maximum: MAX_PORTABLE_PATH_BYTES,
            });
        }
        if value.bytes().any(|byte| byte == 0 || byte == b'\\') {
            return Err(PortablePathError::InvalidSeparator);
        }
        Ok(())
    }

    pub fn from_path(path: &Path) -> Result<Self, PortablePathError> {
        let value = path.to_str().ok_or(PortablePathError::NonUtf8)?;
        #[cfg(windows)]
        let value = value.replace('\\', "/");
        #[cfg(not(windows))]
        let value = value.to_owned();
        Self::new(value)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn require_relative(&self) -> Result<(), PortablePathError> {
        let value = self.as_str();
        let has_drive_prefix = value.as_bytes().get(1) == Some(&b':');
        if value.starts_with('/') || has_drive_prefix {
            return Err(PortablePathError::Absolute);
        }
        if value
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err(PortablePathError::Traversal);
        }
        Ok(())
    }

    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(self.0.replace('/', "\\"))
        }
        #[cfg(not(windows))]
        {
            PathBuf::from(&self.0)
        }
    }
}

impl TryFrom<&Path> for PortablePath {
    type Error = PortablePathError;

    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        Self::from_path(path)
    }
}

impl TryFrom<PathBuf> for PortablePath {
    type Error = PortablePathError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        Self::from_path(&path)
    }
}

impl fmt::Display for PortablePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for PortablePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PortablePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PortablePathError {
    #[error("portable path must not be empty")]
    Empty,
    #[error("portable path contains {actual} UTF-8 bytes; maximum is {maximum}")]
    TooLong { actual: usize, maximum: usize },
    #[error("portable path must be UTF-8")]
    NonUtf8,
    #[error("portable path must use forward slashes and contain no NUL bytes")]
    InvalidSeparator,
    #[error("portable path must be relative in this contract position")]
    Absolute,
    #[error("portable relative path must not contain empty, current, or parent components")]
    Traversal,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireProjectionError {
    #[error("{field} cannot be represented by the fixed-width wire contract")]
    NumericOverflow { field: &'static str },
    #[error("{field} contains a domain variant that is not part of the closed wire contract")]
    UnsupportedVariant { field: &'static str },
    #[error("path cannot be projected into the portable wire contract: {0}")]
    Path(#[from] PortablePathError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationIdV1(DigestV1);

impl GenerationIdV1 {
    #[must_use]
    pub const fn new(digest: DigestV1) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(self) -> DigestV1 {
        self.0
    }
}

impl fmt::Display for GenerationIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for GenerationIdV1 {
    type Err = <DigestV1 as FromStr>::Err;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

impl Serialize for GenerationIdV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GenerationIdV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        DigestV1::deserialize(deserializer).map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationStamp {
    pub protocol_revision: u16,
    pub generation: GenerationIdV1,
    pub workspace: WorkspaceId,
    pub actual_revision: WorkspaceRevision,
    pub desired_revision: WorkspaceRevision,
    pub semantics_current: bool,
    pub configuration_current: bool,
    pub stale: bool,
}

impl GenerationStamp {
    #[must_use]
    pub const fn current(
        generation: GenerationIdV1,
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
    ) -> Self {
        Self {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            generation,
            workspace,
            actual_revision: revision,
            desired_revision: revision,
            semantics_current: true,
            configuration_current: true,
            stale: false,
        }
    }

    #[must_use]
    pub fn with_desired_revision(mut self, desired_revision: WorkspaceRevision) -> Self {
        self.desired_revision = desired_revision;
        self.refresh_stale();
        self
    }

    #[must_use]
    pub fn with_semantics_current(mut self, semantics_current: bool) -> Self {
        self.semantics_current = semantics_current;
        self.refresh_stale();
        self
    }

    #[must_use]
    pub fn with_configuration_current(mut self, configuration_current: bool) -> Self {
        self.configuration_current = configuration_current;
        self.refresh_stale();
        self
    }

    fn refresh_stale(&mut self) {
        self.stale = self.actual_revision != self.desired_revision
            || !self.semantics_current
            || !self.configuration_current;
    }
}

impl ValidateContract for GenerationStamp {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "generation stamp",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        if self.stale
            != (self.actual_revision != self.desired_revision
                || !self.semantics_current
                || !self.configuration_current)
        {
            return Err(ContractValidationError::Inconsistent {
                field: "generation.stale",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FilesystemReindexScope {
    Full,
    Reconcile,
    ChangedPaths { paths: Vec<PortablePath> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemReindexIntent {
    pub protocol_revision: u16,
    pub scope: FilesystemReindexScope,
}

impl FilesystemReindexIntent {
    #[must_use]
    pub const fn full() -> Self {
        Self {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            scope: FilesystemReindexScope::Full,
        }
    }

    #[must_use]
    pub const fn reconcile() -> Self {
        Self {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            scope: FilesystemReindexScope::Reconcile,
        }
    }

    #[must_use]
    pub fn changed_paths(paths: Vec<PortablePath>) -> Self {
        Self {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            scope: FilesystemReindexScope::ChangedPaths { paths },
        }
    }
}

impl ValidateContract for FilesystemReindexIntent {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "filesystem reindex intent",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        if let FilesystemReindexScope::ChangedPaths { paths } = &self.scope {
            if paths.is_empty() {
                return Err(ContractValidationError::Empty {
                    field: "reindex.changed_paths",
                });
            }
            if paths.len() > 4_096 {
                return Err(ContractValidationError::EntryLimit {
                    field: "reindex.changed_paths",
                    actual: paths.len(),
                    maximum: 4_096,
                });
            }
            if paths.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err(ContractValidationError::NotStrictlyIncreasing {
                    field: "reindex.changed_paths",
                });
            }
            if paths.iter().any(|path| path.require_relative().is_err()) {
                return Err(ContractValidationError::Inconsistent {
                    field: "reindex.changed_paths",
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReindexDisposition {
    Applied,
    AlreadyApplied,
    Coalesced,
    Queued,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexReceipt {
    pub protocol_revision: u16,
    pub disposition: ReindexDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction: Option<TransactionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<WorkspaceRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationStamp>,
    pub evidence: ReindexEvidence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexEvidence {
    pub forced_full_scan: bool,
    pub forced_full_analysis: bool,
    pub full_dependency_scan: bool,
    pub dependency_candidate_assets: u64,
    pub dependency_closure_assets: u64,
    pub analysis: ReindexAnalysisEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_estimate: Option<ReindexDiskEstimate>,
    pub publish_warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexAnalysisEvidence {
    pub assets_visited: u64,
    pub assets_analyzed: u64,
    pub source_opens: u64,
    pub source_bytes_read: u64,
    pub text_sources: u64,
    pub text_bytes_scanned: u64,
    pub yaml_documents: u64,
    pub binary_objects: u64,
    pub unity_values_visited: u64,
    pub references_emitted: u64,
    pub container_entries_emitted: u64,
    pub truncations_emitted: u64,
    pub diagnostics_emitted: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexDiskEstimate {
    pub existing_generation_bytes: u64,
    pub old_active_generation_bytes: u64,
    pub new_generation_bytes: u64,
    pub publish_peak_bytes: u64,
    pub retained_bytes_after_publish: u64,
    pub reclaimable_bytes_after_publish: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub failed_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_revision: Option<WorkspaceRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationStatus {
    pub protocol_revision: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<GenerationStamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub building_revision: Option<WorkspaceRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<GenerationFailure>,
}

impl Default for GenerationStatus {
    fn default() -> Self {
        Self {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            active: None,
            building_revision: None,
            last_failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub path: PortablePath,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_id: Option<i32>,
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
    pub generation: GenerationIdV1,
    pub query_policy_id: QueryPolicyId,
    pub after_stable_id: String,
    pub query_binding: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceRequest {
    pub direction: ReferenceDirection,
    pub selector: ReferenceSelector,
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ReferenceCursor>,
}

impl ReferenceRequest {
    #[must_use]
    pub fn incoming_object(address: ObjectAddress, limit: u32) -> Self {
        Self::new(
            ReferenceDirection::Incoming,
            ReferenceSelector::Object { address },
            limit,
        )
    }

    #[must_use]
    pub fn incoming_guid(guid: impl Into<String>, file_id: Option<i64>, limit: u32) -> Self {
        Self::guid(ReferenceDirection::Incoming, guid, file_id, limit)
    }

    #[must_use]
    pub fn outgoing_object(address: ObjectAddress, limit: u32) -> Self {
        Self::new(
            ReferenceDirection::Outgoing,
            ReferenceSelector::Object { address },
            limit,
        )
    }

    #[must_use]
    pub fn outgoing_guid(guid: impl Into<String>, file_id: Option<i64>, limit: u32) -> Self {
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
        limit: u32,
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

    fn new(direction: ReferenceDirection, selector: ReferenceSelector, limit: u32) -> Self {
        Self {
            direction,
            selector,
            limit,
            cursor: None,
        }
    }

    pub fn cursor_query_binding(&self) -> Result<String, ContractValidationError> {
        let mut hasher = Sha256::new();
        hasher.update(REFERENCE_CURSOR_BINDING_DOMAIN);
        hasher.update([match self.direction {
            ReferenceDirection::Incoming => 0,
            ReferenceDirection::Outgoing => 1,
        }]);
        serde_json::to_writer(Sha256Writer(&mut hasher), &self.selector).map_err(|_| {
            ContractValidationError::Inconsistent {
                field: "reference cursor query binding",
            }
        })?;
        Ok(format!(
            "{REFERENCE_CURSOR_BINDING_PREFIX}{}",
            hex::encode(hasher.finalize())
        ))
    }
}

impl ValidateContract for ReferenceRequest {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if self.limit == 0 {
            return Err(ContractValidationError::Empty {
                field: "references.limit",
            });
        }
        if self.limit > MAX_REFERENCE_RESULTS {
            return Err(ContractValidationError::NumericLimit {
                field: "references.limit",
                actual: u64::from(self.limit),
                maximum: u64::from(MAX_REFERENCE_RESULTS),
            });
        }
        if let ReferenceSelector::Guid { guid, .. } = &self.selector {
            validate_guid("references GUID", guid)?;
        }
        if let Some(cursor) = &self.cursor {
            cursor.validate()?;
            if cursor.query_binding != self.cursor_query_binding()? {
                return Err(ContractValidationError::Inconsistent {
                    field: "reference cursor query binding",
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCoverage {
    pub complete: bool,
    pub truncated: bool,
    pub returned: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<ReferenceCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceDiagnosticCoverage {
    pub returned: u32,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub serialized_bytes: u64,
    pub max_count: u32,
    pub max_serialized_bytes: u64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    InvalidRequest,
    InvalidCursor,
    StaleCursor,
    IncompatibleProtocol,
    PeerRejected,
    Busy,
    NotReady,
    RevisionMismatch,
    IndexBuildFailed,
    IdempotencyConflict,
    OperationNotFound,
    OperationControlForbidden,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    pub protocol_revision: u16,
    pub code: ApiErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationStamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_policy_id: Option<QueryPolicyId>,
    pub details: BTreeMap<String, String>,
}

impl ApiError {
    #[must_use]
    pub fn new(code: ApiErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            code,
            message: message.into(),
            retryable,
            generation: None,
            query_policy_id: None,
            details: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_generation(mut self, generation: GenerationStamp) -> Self {
        self.generation = Some(generation);
        self
    }

    #[must_use]
    pub fn with_query_policy(mut self, query_policy_id: QueryPolicyId) -> Self {
        self.query_policy_id = Some(query_policy_id);
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
    pub protocol_revision: u16,
    pub search: bool,
    pub suggest: bool,
    pub incoming_references: bool,
    pub outgoing_references: bool,
    pub filesystem_reindex: bool,
    pub reindex_lifecycle: bool,
    pub background_reindex_discovery: bool,
    pub graceful_shutdown: bool,
}

impl SearchCapabilities {
    #[must_use]
    pub const fn current() -> Self {
        Self {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            search: true,
            suggest: true,
            incoming_references: true,
            outgoing_references: true,
            filesystem_reindex: true,
            reindex_lifecycle: true,
            background_reindex_discovery: true,
            graceful_shutdown: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighlightRangeV1 {
    pub start: u32,
    pub end: u32,
}

impl TryFrom<HighlightRange> for HighlightRangeV1 {
    type Error = WireProjectionError;

    fn try_from(range: HighlightRange) -> Result<Self, Self::Error> {
        Ok(Self {
            start: fixed_u32(range.start, "highlight range start")?,
            end: fixed_u32(range.end, "highlight range end")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TermExplanationV1 {
    pub term: String,
    pub quoted: bool,
    pub kind: MatchKind,
    pub field: MatchField,
}

impl From<TermExplanation> for TermExplanationV1 {
    fn from(explanation: TermExplanation) -> Self {
        Self {
            term: explanation.term,
            quoted: explanation.quoted,
            kind: explanation.kind,
            field: explanation.field,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchExplanationV1 {
    pub terms: Vec<TermExplanationV1>,
    pub fuzzy_fallback: bool,
}

impl From<MatchExplanation> for MatchExplanationV1 {
    fn from(explanation: MatchExplanation) -> Self {
        Self {
            terms: explanation.terms.into_iter().map(Into::into).collect(),
            fuzzy_fallback: explanation.fuzzy_fallback,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankingSignalsV1 {
    pub field_boost: u32,
    pub fuzzy_score: i64,
    pub retrieval_stage: RetrievalStage,
    pub retrieval_score: i64,
}

impl From<RankingSignals> for RankingSignalsV1 {
    fn from(signals: RankingSignals) -> Self {
        Self {
            field_boost: signals.field_boost,
            fuzzy_score: signals.fuzzy_score,
            retrieval_stage: signals.retrieval_stage,
            retrieval_score: signals.retrieval_score,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchCountRelationV1 {
    Exact,
    LowerBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchCountV1 {
    pub value: u64,
    pub relation: MatchCountRelationV1,
}

impl TryFrom<MatchCount> for MatchCountV1 {
    type Error = WireProjectionError;

    fn try_from(count: MatchCount) -> Result<Self, Self::Error> {
        Ok(Self {
            value: fixed_u64(count.value, "match count")?,
            relation: match count.relation {
                MatchCountRelation::Exact => MatchCountRelationV1::Exact,
                MatchCountRelation::LowerBound => MatchCountRelationV1::LowerBound,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FuzzyWorkUsageV1 {
    pub consumed: u64,
    pub limit: u64,
    pub exhausted: bool,
}

impl TryFrom<FuzzyWorkUsage> for FuzzyWorkUsageV1 {
    type Error = WireProjectionError;

    fn try_from(usage: FuzzyWorkUsage) -> Result<Self, Self::Error> {
        Ok(Self {
            consumed: fixed_u64(usage.consumed, "fuzzy work consumed")?,
            limit: fixed_u64(usage.limit, "fuzzy work limit")?,
            exhausted: usage.exhausted,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateFieldV1 {
    StableKey,
    Name,
    Path,
    Kind,
    Guid,
    ContainerSourcePath,
}

impl From<CandidateField> for CandidateFieldV1 {
    fn from(field: CandidateField) -> Self {
        match field {
            CandidateField::StableKey => Self::StableKey,
            CandidateField::Name => Self::Name,
            CandidateField::Path => Self::Path,
            CandidateField::Kind => Self::Kind,
            CandidateField::Guid => Self::Guid,
            CandidateField::ContainerSourcePath => Self::ContainerSourcePath,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case", deny_unknown_fields)]
pub enum SearchDiagnosticV1 {
    EmptyQuery,
    UnterminatedQuote {
        byte_offset: u64,
    },
    EmptyQuotedTerm {
        byte_offset: u64,
    },
    MissingFilterValue {
        field: String,
    },
    DuplicateFilter {
        field: String,
    },
    UnsupportedTypeFilter {
        value: String,
    },
    CandidateLimitExceeded {
        stage: RetrievalStage,
        provided: u64,
        limit: u64,
    },
    QueryByteLimitExceeded {
        actual: u64,
        limit: u64,
    },
    QueryTermLimitExceeded {
        actual: u64,
        limit: u64,
    },
    RetrievalTermLimitExceeded {
        actual: u64,
        limit: u64,
    },
    CandidateFieldByteLimitExceeded {
        field: CandidateFieldV1,
        actual: u64,
        limit: u64,
    },
    CandidateTotalByteLimitExceeded {
        consumed: u64,
        limit: u64,
    },
    CandidateInputLimitExceeded {
        limit: u64,
    },
    CandidateEvidenceLimitExceeded {
        actual: u64,
        limit: u64,
    },
    FuzzyWorkLimitExceeded {
        attempted: u64,
        limit: u64,
    },
    InvalidRetrievalEvidence {
        term_index: u64,
    },
    DuplicateCandidateKey {
        stable_key: String,
    },
}

impl TryFrom<SearchDiagnostic> for SearchDiagnosticV1 {
    type Error = WireProjectionError;

    fn try_from(diagnostic: SearchDiagnostic) -> Result<Self, Self::Error> {
        Ok(match diagnostic {
            SearchDiagnostic::EmptyQuery => Self::EmptyQuery,
            SearchDiagnostic::UnterminatedQuote { byte_offset } => Self::UnterminatedQuote {
                byte_offset: fixed_u64(byte_offset, "diagnostic byte offset")?,
            },
            SearchDiagnostic::EmptyQuotedTerm { byte_offset } => Self::EmptyQuotedTerm {
                byte_offset: fixed_u64(byte_offset, "diagnostic byte offset")?,
            },
            SearchDiagnostic::MissingFilterValue { field } => Self::MissingFilterValue { field },
            SearchDiagnostic::DuplicateFilter { field } => Self::DuplicateFilter { field },
            SearchDiagnostic::UnsupportedTypeFilter { value } => {
                Self::UnsupportedTypeFilter { value }
            }
            SearchDiagnostic::CandidateLimitExceeded {
                stage,
                provided,
                limit,
            } => Self::CandidateLimitExceeded {
                stage,
                provided: fixed_u64(provided, "diagnostic candidate count")?,
                limit: fixed_u64(limit, "diagnostic candidate limit")?,
            },
            SearchDiagnostic::QueryByteLimitExceeded { actual, limit } => {
                Self::QueryByteLimitExceeded {
                    actual: fixed_u64(actual, "diagnostic query bytes")?,
                    limit: fixed_u64(limit, "diagnostic query byte limit")?,
                }
            }
            SearchDiagnostic::QueryTermLimitExceeded { actual, limit } => {
                Self::QueryTermLimitExceeded {
                    actual: fixed_u64(actual, "diagnostic query terms")?,
                    limit: fixed_u64(limit, "diagnostic query term limit")?,
                }
            }
            SearchDiagnostic::RetrievalTermLimitExceeded { actual, limit } => {
                Self::RetrievalTermLimitExceeded {
                    actual: fixed_u64(actual, "diagnostic retrieval terms")?,
                    limit: fixed_u64(limit, "diagnostic retrieval term limit")?,
                }
            }
            SearchDiagnostic::CandidateFieldByteLimitExceeded {
                field,
                actual,
                limit,
            } => Self::CandidateFieldByteLimitExceeded {
                field: field.into(),
                actual: fixed_u64(actual, "diagnostic candidate field bytes")?,
                limit: fixed_u64(limit, "diagnostic candidate field byte limit")?,
            },
            SearchDiagnostic::CandidateTotalByteLimitExceeded { consumed, limit } => {
                Self::CandidateTotalByteLimitExceeded {
                    consumed: fixed_u64(consumed, "diagnostic candidate bytes")?,
                    limit: fixed_u64(limit, "diagnostic candidate byte limit")?,
                }
            }
            SearchDiagnostic::CandidateInputLimitExceeded { limit } => {
                Self::CandidateInputLimitExceeded {
                    limit: fixed_u64(limit, "diagnostic candidate input limit")?,
                }
            }
            SearchDiagnostic::CandidateEvidenceLimitExceeded { actual, limit } => {
                Self::CandidateEvidenceLimitExceeded {
                    actual: fixed_u64(actual, "diagnostic candidate evidence")?,
                    limit: fixed_u64(limit, "diagnostic candidate evidence limit")?,
                }
            }
            SearchDiagnostic::FuzzyWorkLimitExceeded { attempted, limit } => {
                Self::FuzzyWorkLimitExceeded {
                    attempted: fixed_u64(attempted, "diagnostic fuzzy work")?,
                    limit: fixed_u64(limit, "diagnostic fuzzy work limit")?,
                }
            }
            SearchDiagnostic::InvalidRetrievalEvidence { term_index } => {
                Self::InvalidRetrievalEvidence {
                    term_index: fixed_u64(term_index, "diagnostic retrieval term index")?,
                }
            }
            SearchDiagnostic::DuplicateCandidateKey { stable_key } => {
                Self::DuplicateCandidateKey { stable_key }
            }
            SearchDiagnostic::Unknown { .. } => {
                return Err(WireProjectionError::UnsupportedVariant {
                    field: "search diagnostic",
                });
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    pub rank: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    pub path: PortablePath,
    pub name: String,
    pub kind: String,
    pub stable_id: String,
    pub location: Location,
    pub ranking_signals: RankingSignalsV1,
    pub match_kind: MatchKind,
    pub explanation: MatchExplanationV1,
    pub matched_hierarchy_paths: Vec<String>,
    pub matched_script_symbols: Vec<String>,
    pub highlight_path_ranges: Vec<HighlightRangeV1>,
    pub highlight_name_ranges: Vec<HighlightRangeV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResponse {
    pub protocol_revision: u16,
    pub generation: GenerationStamp,
    pub query_policy_id: QueryPolicyId,
    pub query: String,
    pub took_ms: u64,
    pub match_count: MatchCountV1,
    pub returned_hits: u32,
    pub request_limit_truncated: bool,
    pub fuzzy_work: FuzzyWorkUsageV1,
    pub hits: Vec<SearchHit>,
    pub diagnostics: Vec<SearchDiagnosticV1>,
    pub fallback_used: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub field_hints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceHit {
    pub source_path: PortablePath,
    pub source_kind: String,
    pub stable_id: String,
    pub source_object: ObjectAddress,
    pub location: Location,
    pub contexts: Vec<ReferenceContext>,
    pub objects: Vec<ReferenceObject>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferencesResponse {
    pub protocol_revision: u16,
    pub generation: GenerationStamp,
    pub query_policy_id: QueryPolicyId,
    pub request: ReferenceRequest,
    pub took_ms: u64,
    pub coverage: ReferenceCoverage,
    pub hits: Vec<ReferenceHit>,
    pub diagnostics: Vec<Diagnostic>,
    pub diagnostic_coverage: ReferenceDiagnosticCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestResponse {
    pub protocol_revision: u16,
    pub generation: GenerationStamp,
    pub query_policy_id: QueryPolicyId,
    pub prefix: String,
    pub took_ms: u64,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonLifecycleState {
    Booting,
    Serving,
    Draining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonProcessComponent {
    ReindexCoordinator,
    FilesystemWatcher,
    ReconcileTimer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonProcessFailure {
    pub component: DaemonProcessComponent,
    pub cause: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServingAvailability {
    Unavailable,
    Queryable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationFreshness {
    Absent,
    Stale,
    Current,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessMaintenance {
    Managed,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileLifecycle {
    Idle,
    Queued,
    Running,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationMaintenanceState {
    Clean,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationMaintenanceStatus {
    pub state: GenerationMaintenanceState,
    pub last_recovered_entries: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_cleanup_failure: Option<String>,
}

impl GenerationMaintenanceStatus {
    #[must_use]
    pub const fn clean() -> Self {
        Self {
            state: GenerationMaintenanceState::Clean,
            last_recovered_entries: 0,
            last_cleanup_failure: None,
        }
    }
}

impl Default for GenerationMaintenanceStatus {
    fn default() -> Self {
        Self::clean()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatcherLifecycleState {
    Disabled,
    Starting,
    Healthy,
    Failed,
    Retrying,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatcherStatus {
    pub state: WatcherLifecycleState,
    pub retry_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_in_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerLifecycleState {
    Disabled,
    Scheduled,
    Running,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerStatus {
    pub state: TimerLifecycleState,
    pub run_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_in_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonLifecycleStatus {
    pub lifecycle: DaemonLifecycleState,
    pub process_failure: Option<DaemonProcessFailure>,
    pub serving: ServingAvailability,
    pub freshness: GenerationFreshness,
    pub freshness_maintenance: FreshnessMaintenance,
    pub reconcile: ReconcileLifecycle,
    pub generation_maintenance: GenerationMaintenanceStatus,
    pub watcher: WatcherStatus,
    pub timer: TimerStatus,
    pub background_reindex_operations: Vec<BackgroundReindexOperation>,
}

impl DaemonLifecycleStatus {
    #[must_use]
    pub fn unmanaged(generation: &GenerationStatus, indexing: bool) -> Self {
        let (serving, freshness) = serving_and_freshness(generation);
        Self {
            lifecycle: DaemonLifecycleState::Serving,
            process_failure: None,
            serving,
            freshness,
            freshness_maintenance: FreshnessMaintenance::Unmanaged,
            reconcile: if indexing {
                ReconcileLifecycle::Running
            } else if generation.last_failure.is_some() {
                ReconcileLifecycle::Failed
            } else {
                ReconcileLifecycle::Idle
            },
            generation_maintenance: GenerationMaintenanceStatus::clean(),
            watcher: WatcherStatus {
                state: WatcherLifecycleState::Disabled,
                retry_count: 0,
                last_failure: None,
                next_retry_in_ms: None,
            },
            timer: TimerStatus {
                state: TimerLifecycleState::Disabled,
                run_count: 0,
                last_failure: None,
                next_run_in_ms: None,
            },
            background_reindex_operations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    pub protocol_revision: u16,
    pub daemon: DaemonLifecycleStatus,
    pub generation: GenerationStatus,
    pub query_policy_id: QueryPolicyId,
    pub capabilities: SearchCapabilities,
    pub project_root: PortablePath,
    pub generation_root: PortablePath,
    pub scan_roots: Vec<PortablePath>,
    pub indexed_assets: u64,
    pub indexed_search_documents: u64,
    pub indexed_reference_facts: u64,
    pub incomplete_assets: u64,
    pub projection_truncations: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_build_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_build_unix_ms: Option<u64>,
    pub indexing: bool,
}

impl ValidateContract for ReindexReceipt {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "reindex receipt",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        if let Some(generation) = &self.generation {
            generation.validate()?;
        }
        self.evidence.validate()?;
        Ok(())
    }
}

impl ValidateContract for ReindexEvidence {
    fn validate(&self) -> Result<(), ContractValidationError> {
        Self::validate_publish_warnings(&self.publish_warnings)
    }
}

impl ReindexEvidence {
    pub fn validate_publish_warnings(warnings: &[String]) -> Result<(), ContractValidationError> {
        if warnings.len() > MAX_REINDEX_PUBLISH_WARNINGS {
            return Err(ContractValidationError::EntryLimit {
                field: "reindex publish warnings",
                actual: warnings.len(),
                maximum: MAX_REINDEX_PUBLISH_WARNINGS,
            });
        }
        for warning in warnings {
            ensure_nonempty("reindex publish warning", warning)?;
            ensure_byte_limit(
                "reindex publish warning",
                warning,
                MAX_REINDEX_PUBLISH_WARNING_BYTES,
            )?;
        }
        validate_json_limit(
            "reindex publish warnings JSON",
            warnings,
            MAX_REINDEX_PUBLISH_WARNINGS_JSON_BYTES,
        )
    }
}

impl ValidateContract for GenerationFailure {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_nonempty("generation failure code", &self.code)?;
        ensure_nonempty("generation failure message", &self.message)?;
        ensure_byte_limit(
            "generation failure message",
            &self.message,
            MAX_ERROR_MESSAGE_BYTES,
        )
    }
}

impl ValidateContract for GenerationStatus {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "generation status",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        if let Some(active) = &self.active {
            active.validate()?;
        }
        if let Some(failure) = &self.last_failure {
            failure.validate()?;
        }
        Ok(())
    }
}

impl ValidateContract for ReferenceCursor {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_nonempty("reference cursor stable ID", &self.after_stable_id)?;
        ensure_byte_limit("reference cursor stable ID", &self.after_stable_id, 256)?;
        let Some(encoded) = self
            .query_binding
            .strip_prefix(REFERENCE_CURSOR_BINDING_PREFIX)
        else {
            return Err(ContractValidationError::Inconsistent {
                field: "reference cursor query binding",
            });
        };
        if encoded.len() != 64
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContractValidationError::Inconsistent {
                field: "reference cursor query binding",
            });
        }
        Ok(())
    }
}

impl ValidateContract for ReferenceCoverage {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if self.complete != self.total.is_some() {
            return Err(ContractValidationError::Inconsistent {
                field: "reference coverage total availability",
            });
        }
        if !self.complete && !self.truncated {
            return Err(ContractValidationError::Inconsistent {
                field: "reference coverage incompleteness",
            });
        }
        if self.next_cursor.is_some() && !self.truncated {
            return Err(ContractValidationError::Inconsistent {
                field: "reference coverage cursor",
            });
        }
        if let Some(total) = self.total
            && total < u64::from(self.returned)
        {
            return Err(ContractValidationError::Inconsistent {
                field: "reference coverage total",
            });
        }
        if let Some(cursor) = &self.next_cursor {
            cursor.validate()?;
        }
        Ok(())
    }
}

impl ValidateContract for ReferenceDiagnosticCoverage {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if self.returned > self.max_count
            || self.max_count > MAX_REFERENCE_RESPONSE_DIAGNOSTICS
            || self.serialized_bytes > self.max_serialized_bytes
            || self.max_serialized_bytes > MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES
        {
            return Err(ContractValidationError::Inconsistent {
                field: "reference diagnostic coverage limits",
            });
        }
        if let Some(total) = self.total
            && total < u64::from(self.returned)
        {
            return Err(ContractValidationError::Inconsistent {
                field: "reference diagnostic coverage total",
            });
        }
        Ok(())
    }
}

impl ValidateContract for ApiError {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "API error",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        ensure_nonempty("API error message", &self.message)?;
        ensure_byte_limit("API error message", &self.message, MAX_ERROR_MESSAGE_BYTES)?;
        if self.details.len() > 64 {
            return Err(ContractValidationError::EntryLimit {
                field: "API error details",
                actual: self.details.len(),
                maximum: 64,
            });
        }
        for (key, value) in &self.details {
            ensure_nonempty("API error detail key", key)?;
            ensure_byte_limit("API error detail key", key, 256)?;
            ensure_byte_limit("API error detail value", value, 4 * 1024)?;
        }
        if let Some(generation) = &self.generation {
            generation.validate()?;
        }
        validate_json_limit("API error JSON", self, MAX_API_ERROR_JSON_BYTES)
    }
}

impl ValidateContract for SearchCapabilities {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "search capabilities",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )
    }
}

impl ValidateContract for SearchResponse {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "search response",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        self.generation.validate()?;
        ensure_byte_limit("search response query", &self.query, 4 * 1024)?;
        let returned =
            u32::try_from(self.hits.len()).map_err(|_| ContractValidationError::NumericLimit {
                field: "search response hits",
                actual: u64::MAX,
                maximum: 1_000,
            })?;
        if self.returned_hits != returned
            || returned > 1_000
            || self.match_count.value < u64::from(returned)
        {
            return Err(ContractValidationError::Inconsistent {
                field: "search response hit counts",
            });
        }
        if self.diagnostics.len() > MAX_SEARCH_RESPONSE_DIAGNOSTICS {
            return Err(ContractValidationError::EntryLimit {
                field: "search response diagnostics",
                actual: self.diagnostics.len(),
                maximum: MAX_SEARCH_RESPONSE_DIAGNOSTICS,
            });
        }
        for (index, hit) in self.hits.iter().enumerate() {
            let expected_rank =
                u32::try_from(index + 1).map_err(|_| ContractValidationError::Inconsistent {
                    field: "search response hit rank",
                })?;
            if hit.rank != expected_rank {
                return Err(ContractValidationError::Inconsistent {
                    field: "search response hit rank",
                });
            }
            if let Some(guid) = &hit.guid {
                validate_guid("search hit GUID", guid)?;
            }
            if let Some(guid) = &hit.location.guid {
                validate_guid("search hit location GUID", guid)?;
            }
        }
        validate_json_limit(
            "search response hits JSON",
            &self.hits,
            MAX_SEARCH_HITS_JSON_BYTES,
        )?;
        validate_json_limit(
            "search response diagnostics JSON",
            &self.diagnostics,
            MAX_SEARCH_DIAGNOSTICS_JSON_BYTES,
        )?;
        validate_json_limit("search response JSON", self, MAX_SEARCH_RESPONSE_JSON_BYTES)?;
        Ok(())
    }
}

impl SearchResponse {
    pub fn canonical_hit_json_size(hit: &SearchHit) -> Result<u64, ContractValidationError> {
        canonical_json_size(hit).map_err(|_| ContractValidationError::Inconsistent {
            field: "search hit JSON",
        })
    }
}

impl ValidateContract for ReferencesResponse {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "references response",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        self.generation.validate()?;
        self.request.validate()?;
        self.coverage.validate()?;
        self.diagnostic_coverage.validate()?;
        let returned =
            u32::try_from(self.hits.len()).map_err(|_| ContractValidationError::Inconsistent {
                field: "references response hit count",
            })?;
        let diagnostic_count = u32::try_from(self.diagnostics.len()).map_err(|_| {
            ContractValidationError::Inconsistent {
                field: "references response diagnostic count",
            }
        })?;
        if returned != self.coverage.returned
            || diagnostic_count != self.diagnostic_coverage.returned
        {
            return Err(ContractValidationError::Inconsistent {
                field: "references response returned counts",
            });
        }
        let diagnostic_bytes = canonical_json_size(&self.diagnostics).map_err(|_| {
            ContractValidationError::Inconsistent {
                field: "references response diagnostic bytes",
            }
        })?;
        if diagnostic_bytes != self.diagnostic_coverage.serialized_bytes {
            return Err(ContractValidationError::Inconsistent {
                field: "references response diagnostic bytes",
            });
        }
        if let Some(cursor) = &self.request.cursor
            && (cursor.generation != self.generation.generation
                || cursor.query_policy_id != self.query_policy_id)
        {
            return Err(ContractValidationError::Inconsistent {
                field: "references request cursor binding",
            });
        }
        if let Some(cursor) = &self.coverage.next_cursor
            && (cursor.generation != self.generation.generation
                || cursor.query_policy_id != self.query_policy_id
                || cursor.query_binding != self.request.cursor_query_binding()?)
        {
            return Err(ContractValidationError::Inconsistent {
                field: "references response cursor binding",
            });
        }
        for hit in &self.hits {
            let file_id = hit.source_object.binary_path_id().or_else(|| {
                hit.source_object
                    .yaml_file_id()
                    .map(unity_asset_core::YamlFileId::get)
            });
            if hit.location.path != hit.source_path
                || hit.location.file_id != file_id
                || hit
                    .contexts
                    .iter()
                    .any(|context| context.doc_file_id != file_id)
            {
                return Err(ContractValidationError::Inconsistent {
                    field: "reference hit source identity",
                });
            }
        }
        Ok(())
    }
}

struct Sha256Writer<'hasher>(&'hasher mut Sha256);

impl Write for Sha256Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct JsonSizeWriter {
    bytes: u64,
}

impl Write for JsonSizeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = bytes.len();
        let bytes =
            u64::try_from(written).map_err(|_| io::Error::other("JSON size does not fit u64"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("JSON size overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn canonical_json_size<T: Serialize + ?Sized>(value: &T) -> Result<u64, serde_json::Error> {
    let mut writer = JsonSizeWriter::default();
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

impl ValidateContract for SuggestResponse {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "suggest response",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        self.generation.validate()?;
        ensure_byte_limit("suggest response prefix", &self.prefix, 4 * 1024)?;
        Self::validate_suggestions(&self.suggestions)
    }
}

impl SuggestResponse {
    pub fn validate_suggestion(suggestion: &str) -> Result<(), ContractValidationError> {
        ensure_nonempty("suggest response suggestion", suggestion)?;
        ensure_byte_limit(
            "suggest response suggestion",
            suggestion,
            MAX_SUGGESTION_BYTES,
        )
    }

    pub fn validate_suggestions(suggestions: &[String]) -> Result<(), ContractValidationError> {
        if suggestions.len() > 50 {
            return Err(ContractValidationError::EntryLimit {
                field: "suggest response suggestions",
                actual: suggestions.len(),
                maximum: 50,
            });
        }
        for suggestion in suggestions {
            Self::validate_suggestion(suggestion)?;
        }
        let encoded_bytes = canonical_json_size(suggestions).map_err(|_| {
            ContractValidationError::Inconsistent {
                field: "suggest response suggestions JSON",
            }
        })?;
        if encoded_bytes > MAX_SUGGESTIONS_JSON_BYTES {
            return Err(ContractValidationError::NumericLimit {
                field: "suggest response suggestions JSON",
                actual: encoded_bytes,
                maximum: MAX_SUGGESTIONS_JSON_BYTES,
            });
        }
        Ok(())
    }
}

impl ValidateContract for StatusResponse {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_revision(
            "status response",
            self.protocol_revision,
            SEARCH_PROTOCOL_REVISION,
        )?;
        self.generation.validate()?;
        self.validate_daemon_status()?;
        self.capabilities.validate()?;
        if !self.indexing && self.generation.building_revision.is_some() {
            return Err(ContractValidationError::Inconsistent {
                field: "status response indexing state",
            });
        }
        Self::validate_paths(&self.project_root, &self.generation_root, &self.scan_roots)
    }
}

impl StatusResponse {
    fn validate_daemon_status(&self) -> Result<(), ContractValidationError> {
        let (expected_serving, expected_freshness) = serving_and_freshness(&self.generation);
        if self.daemon.serving != expected_serving {
            return Err(ContractValidationError::Inconsistent {
                field: "daemon serving availability",
            });
        }
        if self.daemon.freshness != expected_freshness {
            return Err(ContractValidationError::Inconsistent {
                field: "daemon generation freshness",
            });
        }
        self.validate_process_failure()?;
        if matches!(
            self.daemon.generation_maintenance.state,
            GenerationMaintenanceState::RecoveryRequired
        ) != self
            .daemon
            .generation_maintenance
            .last_cleanup_failure
            .is_some()
        {
            return Err(ContractValidationError::Inconsistent {
                field: "generation maintenance failure evidence",
            });
        }
        if matches!(
            self.daemon.watcher.state,
            WatcherLifecycleState::Failed | WatcherLifecycleState::Retrying
        ) && self.daemon.watcher.last_failure.is_none()
        {
            return Err(ContractValidationError::Inconsistent {
                field: "watcher failure evidence",
            });
        }
        if matches!(self.daemon.watcher.state, WatcherLifecycleState::Retrying)
            != self.daemon.watcher.next_retry_in_ms.is_some()
        {
            return Err(ContractValidationError::Inconsistent {
                field: "watcher retry deadline",
            });
        }
        if matches!(self.daemon.timer.state, TimerLifecycleState::Failed)
            && self.daemon.timer.last_failure.is_none()
        {
            return Err(ContractValidationError::Inconsistent {
                field: "timer failure evidence",
            });
        }
        if matches!(
            self.daemon.timer.state,
            TimerLifecycleState::Disabled | TimerLifecycleState::Stopped
        ) && self.daemon.timer.next_run_in_ms.is_some()
        {
            return Err(ContractValidationError::Inconsistent {
                field: "disabled timer next run",
            });
        }
        if matches!(self.daemon.timer.state, TimerLifecycleState::Scheduled)
            && self.daemon.timer.next_run_in_ms.is_none()
        {
            return Err(ContractValidationError::Inconsistent {
                field: "scheduled timer next run",
            });
        }
        let expected_maintenance =
            if matches!(self.daemon.watcher.state, WatcherLifecycleState::Disabled)
                && matches!(self.daemon.timer.state, TimerLifecycleState::Disabled)
            {
                FreshnessMaintenance::Unmanaged
            } else {
                FreshnessMaintenance::Managed
            };
        if self.daemon.freshness_maintenance != expected_maintenance {
            return Err(ContractValidationError::Inconsistent {
                field: "freshness maintenance",
            });
        }
        validate_background_reindex_operations(&self.daemon.background_reindex_operations)?;
        for (field, failure) in [
            (
                "generation maintenance last cleanup failure",
                &self.daemon.generation_maintenance.last_cleanup_failure,
            ),
            ("watcher last failure", &self.daemon.watcher.last_failure),
            ("timer last failure", &self.daemon.timer.last_failure),
        ] {
            if let Some(failure) = failure {
                ensure_nonempty(field, failure)?;
                ensure_byte_limit(field, failure, MAX_ERROR_MESSAGE_BYTES)?;
            }
        }
        Ok(())
    }

    fn validate_process_failure(&self) -> Result<(), ContractValidationError> {
        let Some(failure) = &self.daemon.process_failure else {
            return Ok(());
        };
        ensure_nonempty("daemon process failure cause", &failure.cause)?;
        ensure_byte_limit(
            "daemon process failure cause",
            &failure.cause,
            MAX_ERROR_MESSAGE_BYTES,
        )?;
        if self.daemon.lifecycle != DaemonLifecycleState::Draining
            || self.daemon.reconcile != ReconcileLifecycle::Failed
        {
            return Err(ContractValidationError::Inconsistent {
                field: "daemon process failure lifecycle",
            });
        }
        match failure.component {
            DaemonProcessComponent::ReindexCoordinator => Ok(()),
            DaemonProcessComponent::FilesystemWatcher
                if self.daemon.watcher.state == WatcherLifecycleState::Failed
                    && self.daemon.watcher.last_failure.as_deref()
                        == Some(failure.cause.as_str()) =>
            {
                Ok(())
            }
            DaemonProcessComponent::ReconcileTimer
                if self.daemon.timer.state == TimerLifecycleState::Failed
                    && self.daemon.timer.last_failure.as_deref()
                        == Some(failure.cause.as_str()) =>
            {
                Ok(())
            }
            DaemonProcessComponent::FilesystemWatcher | DaemonProcessComponent::ReconcileTimer => {
                Err(ContractValidationError::Inconsistent {
                    field: "daemon process failure component evidence",
                })
            }
        }
    }

    pub fn validate_paths(
        project_root: &PortablePath,
        generation_root: &PortablePath,
        scan_roots: &[PortablePath],
    ) -> Result<(), ContractValidationError> {
        if scan_roots.len() > MAX_STATUS_SCAN_ROOTS {
            return Err(ContractValidationError::EntryLimit {
                field: "status response scan roots",
                actual: scan_roots.len(),
                maximum: MAX_STATUS_SCAN_ROOTS,
            });
        }

        let encoded_bytes = [
            canonical_json_size(project_root),
            canonical_json_size(generation_root),
            canonical_json_size(scan_roots),
        ]
        .into_iter()
        .try_fold(0_u64, |total, encoded| {
            total
                .checked_add(encoded.map_err(|_| ContractValidationError::Inconsistent {
                    field: "status response paths JSON",
                })?)
                .ok_or(ContractValidationError::Inconsistent {
                    field: "status response paths JSON",
                })
        })?;
        if encoded_bytes > MAX_STATUS_PATHS_JSON_BYTES {
            return Err(ContractValidationError::NumericLimit {
                field: "status response paths JSON",
                actual: encoded_bytes,
                maximum: MAX_STATUS_PATHS_JSON_BYTES,
            });
        }
        Ok(())
    }
}

fn serving_and_freshness(
    generation: &GenerationStatus,
) -> (ServingAvailability, GenerationFreshness) {
    match generation.active.as_ref() {
        None => (
            ServingAvailability::Unavailable,
            GenerationFreshness::Absent,
        ),
        Some(active) if active.stale => {
            (ServingAvailability::Queryable, GenerationFreshness::Stale)
        }
        Some(_) => (ServingAvailability::Queryable, GenerationFreshness::Current),
    }
}

fn ensure_nonempty(field: &'static str, value: &str) -> Result<(), ContractValidationError> {
    if value.is_empty() {
        Err(ContractValidationError::Empty { field })
    } else {
        Ok(())
    }
}

fn ensure_byte_limit(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), ContractValidationError> {
    if value.len() > maximum {
        Err(ContractValidationError::ByteLimit {
            field,
            actual: value.len(),
            maximum,
        })
    } else {
        Ok(())
    }
}

fn validate_guid(field: &'static str, guid: &str) -> Result<(), ContractValidationError> {
    if guid.len() == 32
        && guid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ContractValidationError::Inconsistent { field })
    }
}

fn validate_json_limit<T: Serialize + ?Sized>(
    field: &'static str,
    value: &T,
    maximum: u64,
) -> Result<(), ContractValidationError> {
    let actual =
        canonical_json_size(value).map_err(|_| ContractValidationError::Inconsistent { field })?;
    if actual > maximum {
        Err(ContractValidationError::NumericLimit {
            field,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn fixed_u32(value: usize, field: &'static str) -> Result<u32, WireProjectionError> {
    u32::try_from(value).map_err(|_| WireProjectionError::NumericOverflow { field })
}

fn fixed_u64(value: usize, field: &'static str) -> Result<u64, WireProjectionError> {
    u64::try_from(value).map_err(|_| WireProjectionError::NumericOverflow { field })
}
