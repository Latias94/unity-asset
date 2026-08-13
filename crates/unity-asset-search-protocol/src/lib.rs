//! Strict, transport-independent contracts shared by local search clients and daemons.

mod bootstrap;
mod framing;
mod ids;
mod model;
mod operation;
mod validation;

pub use bootstrap::{
    BOOTSTRAP_VERSION, BootstrapErrorCode, BootstrapHelloV2, BootstrapReplyV2,
    MAX_BOOTSTRAP_REVISIONS,
};
pub use framing::{
    FrameLimits, FramingError, decode_frame, decode_request_frame, decode_response_frame,
    decode_validated_frame, encode_frame, encode_request_frame, encode_response_frame,
};
pub use ids::{
    DaemonInstanceId, FixedIdParseError, OperationId, ProjectId, QueryPolicyId, RequestId,
};
pub use model::{
    ApiError, ApiErrorCode, CandidateFieldV1, DaemonLifecycleState, DaemonLifecycleStatus,
    FilesystemReindexIntent, FilesystemReindexScope, FreshnessMaintenance, FuzzyWorkUsageV1,
    GenerationFailure, GenerationFreshness, GenerationIdV1, GenerationMaintenanceState,
    GenerationMaintenanceStatus, GenerationStamp, GenerationStatus, HighlightRangeV1, Location,
    MAX_API_ERROR_JSON_BYTES, MAX_ERROR_MESSAGE_BYTES, MAX_PORTABLE_PATH_BYTES,
    MAX_REFERENCE_RESPONSE_DIAGNOSTIC_JSON_BYTES, MAX_REFERENCE_RESPONSE_DIAGNOSTICS,
    MAX_REINDEX_PUBLISH_WARNING_BYTES, MAX_REINDEX_PUBLISH_WARNINGS,
    MAX_REINDEX_PUBLISH_WARNINGS_JSON_BYTES, MAX_SEARCH_DIAGNOSTICS_JSON_BYTES,
    MAX_SEARCH_HITS_JSON_BYTES, MAX_SEARCH_RESPONSE_DIAGNOSTICS, MAX_SEARCH_RESPONSE_JSON_BYTES,
    MAX_STATUS_PATHS_JSON_BYTES, MAX_STATUS_SCAN_ROOTS, MAX_SUGGESTION_BYTES,
    MAX_SUGGESTIONS_JSON_BYTES, MatchCountRelationV1, MatchCountV1, MatchExplanationV1,
    PortablePath, PortablePathError, RankingSignalsV1, ReconcileLifecycle, ReferenceContext,
    ReferenceCoverage, ReferenceCursor, ReferenceDiagnosticCoverage, ReferenceDirection,
    ReferenceHit, ReferenceObject, ReferenceRequest, ReferenceSelector, ReferencesResponse,
    ReindexAnalysisEvidence, ReindexDiskEstimate, ReindexDisposition, ReindexEvidence,
    ReindexReceipt, SEARCH_PROTOCOL_REVISION, SearchCapabilities, SearchDiagnosticV1, SearchHit,
    SearchResponse, ServingAvailability, StatusResponse, SuggestResponse, TermExplanationV1,
    TimerLifecycleState, TimerStatus, WatcherLifecycleState, WatcherStatus, WireProjectionError,
};
pub use operation::{
    BUSINESS_PROTOCOL_REVISION, BackgroundReindexOperation, BackgroundReindexOrigin,
    CapabilitiesRequest, CapabilitiesResponse, MAX_BACKGROUND_REINDEX_OPERATIONS,
    MAX_IDEMPOTENCY_KEY_BYTES, MAX_REFERENCE_RESULTS, MAX_SEARCH_QUERY_BYTES, MAX_SEARCH_RESULTS,
    MAX_SHUTDOWN_DRAIN_MS, MAX_SUGGEST_PREFIX_BYTES, MAX_SUGGEST_RESULTS, MAX_WAIT_TIMEOUT_MS,
    OperationKind, ReindexAdmitRequest, ReindexCancelRequest, ReindexCancelResponse,
    ReindexOperationState, ReindexOperationStatus, ReindexStatusRequest, ReindexWaitRequest,
    RequestEnvelope, RequestOperation, ResponseEnvelope, ResponseOperation, ResponseOutcome,
    SearchRequest, ShutdownRequest, ShutdownResponse, StatusRequest, SuggestRequest,
};
pub use validation::{ContractValidationError, ValidateContract};
