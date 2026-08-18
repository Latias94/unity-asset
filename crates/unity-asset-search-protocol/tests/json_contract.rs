use unity_asset_core::AssetLoadBudget;
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, BUSINESS_PROTOCOL_REVISION, DaemonInstanceId, OperationKind, ProjectId,
    ProtocolJsonError, QueryPolicyId, RequestEnvelope, RequestId, RequestOperation,
    ResponseEnvelope, ResponseOperation, ResponseOutcome, SearchRequest, SearchResponse,
    decode_request_json, decode_response_json, encode_request_json, encode_response_json,
    max_response_json_bytes,
};

fn request_for(operation: RequestOperation) -> RequestEnvelope {
    RequestEnvelope::new(
        BUSINESS_PROTOCOL_REVISION,
        RequestId::from_bytes([0x11; 16]),
        ProjectId::from_bytes([0x22; 32]),
        DaemonInstanceId::from_bytes([0x33; 16]),
        QueryPolicyId::from_bytes([0x44; 32]),
        operation,
    )
    .unwrap()
}

fn request() -> RequestEnvelope {
    request_for(RequestOperation::Search(SearchRequest {
        query: "player".to_owned(),
        limit: 25,
    }))
}

fn response_encoder(request: &RequestEnvelope) -> unity_asset_search_protocol::ResponseEncoder {
    let encoded = encode_request_json(request).unwrap();
    let mut budget = AssetLoadBudget::default();
    let validated = decode_request_json(&encoded, &mut budget).unwrap();
    let (operation, response_encoder) = validated
        .bind(
            request.project_id(),
            request.daemon_instance_id(),
            request.query_policy_id(),
        )
        .unwrap();
    assert_eq!(&operation, request.operation());
    response_encoder
}

fn fixture_search_response() -> SearchResponse {
    let envelope: ResponseEnvelope = serde_json::from_str(include_str!(
        "../../../integration/search-protocol/fixtures/responses/search-v1.json"
    ))
    .unwrap();
    let ResponseOutcome::Success(operation) = envelope.into_outcome() else {
        panic!("search fixture must be successful");
    };
    let ResponseOperation::Search(response) = *operation else {
        panic!("search fixture must contain a search response");
    };
    response
}

#[test]
fn request_json_round_trips_without_transport_framing() {
    let expected = request();
    let encoded = encode_request_json(&expected).unwrap();

    assert_eq!(encoded.first(), Some(&b'{'));
    let mut budget = AssetLoadBudget::default();
    let decoded = decode_request_json(&encoded, &mut budget).unwrap();
    let (operation, _) = decoded
        .bind(
            expected.project_id(),
            expected.daemon_instance_id(),
            expected.query_policy_id(),
        )
        .unwrap();

    assert_eq!(&operation, expected.operation());
}

#[test]
fn decoded_request_binding_rejects_each_changed_scalar() {
    let expected = request();
    let encoded = encode_request_json(&expected).unwrap();
    let bindings = [
        (
            ProjectId::from_bytes([0x99; 32]),
            expected.daemon_instance_id(),
            expected.query_policy_id(),
        ),
        (
            expected.project_id(),
            DaemonInstanceId::from_bytes([0x99; 16]),
            expected.query_policy_id(),
        ),
        (
            expected.project_id(),
            expected.daemon_instance_id(),
            QueryPolicyId::from_bytes([0x99; 32]),
        ),
    ];

    for (project, instance, query_policy) in bindings {
        let mut budget = AssetLoadBudget::default();
        let decoded = decode_request_json(&encoded, &mut budget).unwrap();
        assert!(decoded.bind(project, instance, query_policy).is_err());
    }
}

#[test]
fn request_json_rejects_noncanonical_spelling() {
    let expected = request();
    let canonical = encode_request_json(&expected).unwrap();
    let mut noncanonical = Vec::with_capacity(canonical.len() + 1);
    noncanonical.extend_from_slice(b"{ ");
    noncanonical.extend_from_slice(&canonical[1..]);

    let mut budget = AssetLoadBudget::default();
    assert!(matches!(
        decode_request_json(&noncanonical, &mut budget),
        Err(ProtocolJsonError::NonCanonicalJson)
    ));
}

#[test]
fn response_json_limits_are_available_by_operation() {
    const SMALL_RESPONSE_JSON_BYTES: usize = 256 * 1024;
    const LARGE_RESPONSE_JSON_BYTES: usize = 16 * 1024 * 1024;

    let expected = [
        (OperationKind::Capabilities, SMALL_RESPONSE_JSON_BYTES),
        (OperationKind::Status, SMALL_RESPONSE_JSON_BYTES),
        (OperationKind::Search, LARGE_RESPONSE_JSON_BYTES),
        (OperationKind::Suggest, SMALL_RESPONSE_JSON_BYTES),
        (OperationKind::References, LARGE_RESPONSE_JSON_BYTES),
        (OperationKind::ReindexAdmit, LARGE_RESPONSE_JSON_BYTES),
        (OperationKind::ReindexStatus, LARGE_RESPONSE_JSON_BYTES),
        (OperationKind::ReindexWait, LARGE_RESPONSE_JSON_BYTES),
        (OperationKind::ReindexCancel, SMALL_RESPONSE_JSON_BYTES),
        (OperationKind::Shutdown, SMALL_RESPONSE_JSON_BYTES),
    ];

    for (operation, maximum) in expected {
        assert_eq!(max_response_json_bytes(operation), maximum);
    }
}

#[test]
fn response_json_remains_bound_to_its_request() {
    let request = request();
    let expected = ResponseEnvelope::error(
        &request,
        ApiError::new(ApiErrorCode::NotReady, "index is not ready", true)
            .with_query_policy(request.query_policy_id()),
    );
    let encoded = encode_response_json(&expected, &request).unwrap();

    assert_eq!(encoded.first(), Some(&b'{'));
    let mut budget = AssetLoadBudget::default();
    let decoded = decode_response_json(&encoded, &mut budget, &request).unwrap();

    assert_eq!(decoded, expected);
}

#[test]
fn response_result_json_reuses_the_existing_request_binding_contract() {
    let request = request();
    let mut valid = fixture_search_response();
    valid.query = "player".to_owned();
    valid.query_policy_id = request.query_policy_id();

    let encoded = response_encoder(&request)
        .encode(Ok(ResponseOperation::Search(valid.clone())))
        .unwrap();
    let mut budget = AssetLoadBudget::default();
    decode_response_json(&encoded, &mut budget, &request).unwrap();

    let mut wrong_query = valid;
    wrong_query.query = "different".to_owned();
    assert!(matches!(
        response_encoder(&request).encode(Ok(ResponseOperation::Search(wrong_query))),
        Err(ProtocolJsonError::Validation(_))
    ));
}
