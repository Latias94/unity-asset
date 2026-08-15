//! Public API contract for the `unity-asset-search-protocol` package.

pub use unity_asset_search_protocol::{
    ApiErrorCode, BackgroundReindexOperation, BackgroundReindexOrigin, ContractValidationError,
    DaemonInstanceId, GenerationStamp, Location, OperationId, OperationKind, PortablePath,
    ProjectId, QueryPolicyId, ReferenceRequest, ReindexOperationState, RequestEnvelope, RequestId,
    RequestOperation, ResponseEnvelope, ResponseOperation, SEARCH_PROTOCOL_REVISION,
    SearchCapabilities, SearchRequest, SearchResponse, StatusResponse, ValidateContract,
    decode_request_json, decode_response_json, encode_request_json, encode_response_json,
};
