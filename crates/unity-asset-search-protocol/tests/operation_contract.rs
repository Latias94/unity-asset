use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, BUSINESS_PROTOCOL_REVISION, CapabilitiesRequest, DaemonInstanceId,
    FilesystemReindexIntent, OperationId, ProjectId, QueryPolicyId, ReferenceRequest,
    ReindexAdmitRequest, ReindexCancelRequest, ReindexOperationState, ReindexOperationStatus,
    ReindexStatusRequest, ReindexWaitRequest, RequestEnvelope, RequestId, RequestOperation,
    ResponseEnvelope, ResponseOperation, SearchRequest, ShutdownRequest, ShutdownResponse,
    StatusRequest, SuggestRequest, ValidateContract,
};

const GUID: &str = "0123456789abcdef0123456789abcdef";

fn envelope(operation: RequestOperation) -> RequestEnvelope {
    RequestEnvelope::new(
        BUSINESS_PROTOCOL_REVISION,
        RequestId::from_bytes([1; 16]),
        ProjectId::from_bytes([2; 32]),
        DaemonInstanceId::from_bytes([3; 16]),
        QueryPolicyId::from_bytes([4; 32]),
        operation,
    )
    .unwrap()
}

#[test]
fn every_request_operation_has_one_strict_nonempty_wire_variant() {
    let operation_id = OperationId::from_bytes([5; 16]);
    let operations = vec![
        RequestOperation::Capabilities(CapabilitiesRequest {}),
        RequestOperation::Status(StatusRequest {}),
        RequestOperation::Search(SearchRequest {
            query: "player".to_owned(),
            limit: 25,
        }),
        RequestOperation::Suggest(SuggestRequest {
            prefix: "pla".to_owned(),
            limit: 10,
        }),
        RequestOperation::References(ReferenceRequest::outgoing_guid(GUID, Some(1), 25)),
        RequestOperation::ReindexAdmit(ReindexAdmitRequest {
            intent: FilesystemReindexIntent::full(),
            idempotency_key: Some("agent-request-1".to_owned()),
        }),
        RequestOperation::ReindexStatus(ReindexStatusRequest { operation_id }),
        RequestOperation::ReindexWait(ReindexWaitRequest {
            operation_id,
            timeout_ms: 1_000,
        }),
        RequestOperation::ReindexCancel(ReindexCancelRequest { operation_id }),
        RequestOperation::Shutdown(ShutdownRequest {
            drain_timeout_ms: 5_000,
        }),
    ];

    for operation in operations {
        let expected = envelope(operation);
        let encoded = serde_json::to_vec(&expected).unwrap();
        let decoded: RequestEnvelope = serde_json::from_slice(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded, expected);
    }
}

#[test]
fn search_zero_is_empty_by_contract_and_over_limit_is_rejected() {
    envelope(RequestOperation::Search(SearchRequest {
        query: "anything".to_owned(),
        limit: 0,
    }));

    assert!(
        RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([1; 16]),
            ProjectId::from_bytes([2; 32]),
            DaemonInstanceId::from_bytes([3; 16]),
            QueryPolicyId::from_bytes([4; 32]),
            RequestOperation::Search(SearchRequest {
                query: "anything".to_owned(),
                limit: 1_001,
            }),
        )
        .is_err()
    );
}

#[test]
fn suggest_limit_is_nonzero_and_bounded() {
    assert!(
        RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([1; 16]),
            ProjectId::from_bytes([2; 32]),
            DaemonInstanceId::from_bytes([3; 16]),
            QueryPolicyId::from_bytes([4; 32]),
            RequestOperation::Suggest(SuggestRequest {
                prefix: "pla".to_owned(),
                limit: 0,
            }),
        )
        .is_err()
    );
    envelope(RequestOperation::Suggest(SuggestRequest {
        prefix: "pla".to_owned(),
        limit: 50,
    }));
    assert!(
        RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            RequestId::from_bytes([1; 16]),
            ProjectId::from_bytes([2; 32]),
            DaemonInstanceId::from_bytes([3; 16]),
            QueryPolicyId::from_bytes([4; 32]),
            RequestOperation::Suggest(SuggestRequest {
                prefix: "pla".to_owned(),
                limit: 51,
            }),
        )
        .is_err()
    );
}

#[test]
fn successful_response_must_match_the_request_operation() {
    let request = envelope(RequestOperation::Search(SearchRequest {
        query: "player".to_owned(),
        limit: 10,
    }));
    let response = ResponseEnvelope::success(
        &request,
        ResponseOperation::Shutdown(ShutdownResponse { accepted: true }),
    );

    assert!(response.validate_for(&request).is_err());

    let error = ResponseEnvelope::error(
        &request,
        ApiError::new(ApiErrorCode::NotReady, "index is booting", true),
    );
    error.validate_for(&request).unwrap();
}

#[test]
fn reindex_terminal_fields_cannot_contradict_state() {
    let operation_id = OperationId::from_bytes([5; 16]);
    let queued = ReindexOperationStatus {
        operation_id,
        state: ReindexOperationState::Queued,
        admission: None,
        completion: None,
        status: None,
        error: None,
    };
    queued.validate().unwrap();

    let failed_without_error = ReindexOperationStatus {
        state: ReindexOperationState::Failed,
        ..queued
    };
    assert!(failed_without_error.validate().is_err());
}
