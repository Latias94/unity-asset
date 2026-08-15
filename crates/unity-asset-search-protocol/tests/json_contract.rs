use unity_asset_core::AssetLoadBudget;
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, BUSINESS_PROTOCOL_REVISION, DaemonInstanceId, ProjectId,
    ProtocolJsonError, QueryPolicyId, RequestEnvelope, RequestId, RequestOperation,
    ResponseEnvelope, SearchRequest, decode_request_json, decode_response_json,
    encode_request_json, encode_response_json,
};

fn request() -> RequestEnvelope {
    RequestEnvelope::new(
        BUSINESS_PROTOCOL_REVISION,
        RequestId::from_bytes([0x11; 16]),
        ProjectId::from_bytes([0x22; 32]),
        DaemonInstanceId::from_bytes([0x33; 16]),
        QueryPolicyId::from_bytes([0x44; 32]),
        RequestOperation::Search(SearchRequest {
            query: "player".to_owned(),
            limit: 25,
        }),
    )
    .unwrap()
}

#[test]
fn request_json_round_trips_without_transport_framing() {
    let expected = request();
    let encoded = encode_request_json(&expected).unwrap();

    assert_eq!(encoded.first(), Some(&b'{'));
    let mut budget = AssetLoadBudget::default();
    let decoded = decode_request_json(&encoded, &mut budget).unwrap();

    assert_eq!(decoded, expected);
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
