//! Public API contract for the `unity-asset-search-protocol` package.

pub use unity_asset_search_protocol::{
    ApiErrorCode, BOOTSTRAP_VERSION, BackgroundReindexOperation, BackgroundReindexOrigin,
    BootstrapHelloV2, BootstrapReplyV2, ContractValidationError, DaemonInstanceId, FrameLimits,
    GenerationStamp, Location, OperationId, OperationKind, PortablePath, ProjectId, QueryPolicyId,
    ReferenceRequest, ReindexOperationState, RequestEnvelope, RequestId, RequestOperation,
    ResponseEnvelope, ResponseOperation, SEARCH_PROTOCOL_REVISION, SearchCapabilities,
    SearchRequest, SearchResponse, StatusResponse, ValidateContract, decode_request_frame,
    decode_response_frame, encode_request_frame, encode_response_frame,
};
