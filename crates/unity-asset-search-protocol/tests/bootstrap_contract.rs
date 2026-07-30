use unity_asset_core::AssetLoadBudget;
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, BOOTSTRAP_VERSION, BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2,
    BootstrapReplyV2, DaemonInstanceId, FrameLimits, FramingError, ProjectId, QueryPolicyId,
    ReferenceRequest, RequestEnvelope, RequestId, RequestOperation, ResponseEnvelope,
    SearchRequest, decode_frame, decode_request_frame, decode_response_frame, encode_frame,
    encode_request_frame, encode_response_frame,
};

fn project_id() -> ProjectId {
    ProjectId::from_bytes([0x11; 32])
}

fn instance_id() -> DaemonInstanceId {
    DaemonInstanceId::from_bytes([0x22; 16])
}

fn query_policy_id() -> QueryPolicyId {
    QueryPolicyId::from_bytes([0x55; 32])
}

#[test]
fn bootstrap_is_frozen_before_business_revision_negotiation() {
    let hello = BootstrapHelloV2::new(
        project_id(),
        instance_id(),
        vec![BUSINESS_PROTOCOL_REVISION],
    )
    .unwrap();
    let encoded = encode_frame(&hello, FrameLimits::bootstrap()).unwrap();
    let mut budget = AssetLoadBudget::default();
    let decoded: BootstrapHelloV2 =
        decode_frame(&encoded, &mut budget, FrameLimits::bootstrap()).unwrap();

    assert_eq!(decoded, hello);
    assert_eq!(decoded.bootstrap_version(), BOOTSTRAP_VERSION);
    assert_eq!(decoded.supported_revisions(), &[BUSINESS_PROTOCOL_REVISION]);

    let reply = BootstrapReplyV2::negotiate(
        &decoded,
        project_id(),
        instance_id(),
        query_policy_id(),
        &[BUSINESS_PROTOCOL_REVISION],
    );
    assert_eq!(reply.selected_revision(), Some(BUSINESS_PROTOCOL_REVISION));
    assert_eq!(reply.query_policy_id(), Some(query_policy_id()));
    reply.validate_for(&decoded).unwrap();
}

#[test]
fn bootstrap_rejects_unsorted_duplicate_and_unbound_inputs() {
    assert!(BootstrapHelloV2::new(project_id(), instance_id(), vec![]).is_err());
    assert!(BootstrapHelloV2::new(project_id(), instance_id(), vec![2, 1]).is_err());
    assert!(BootstrapHelloV2::new(project_id(), instance_id(), vec![1, 1]).is_err());

    let hello = BootstrapHelloV2::new(
        project_id(),
        instance_id(),
        vec![BUSINESS_PROTOCOL_REVISION],
    )
    .unwrap();
    assert!(
        BootstrapReplyV2::negotiate(
            &hello,
            ProjectId::from_bytes([0x33; 32]),
            instance_id(),
            query_policy_id(),
            &[BUSINESS_PROTOCOL_REVISION],
        )
        .selected_revision()
        .is_none()
    );
    assert!(
        BootstrapReplyV2::negotiate(&hello, project_id(), instance_id(), query_policy_id(), &[1],)
            .selected_revision()
            .is_none()
    );
}

#[test]
fn framed_json_is_exact_and_rejects_oversized_declared_lengths() {
    let hello = BootstrapHelloV2::new(
        project_id(),
        instance_id(),
        vec![BUSINESS_PROTOCOL_REVISION],
    )
    .unwrap();
    let limits = FrameLimits::bootstrap();
    let mut encoded = encode_frame(&hello, limits).unwrap();
    encoded.extend_from_slice(b"{}");

    let mut budget = AssetLoadBudget::default();
    assert!(decode_frame::<BootstrapHelloV2>(&encoded, &mut budget, limits).is_err());

    let oversized = u32::try_from(limits.max_encoded_bytes() + 1)
        .unwrap()
        .to_be_bytes();
    let mut budget = AssetLoadBudget::default();
    assert!(decode_frame::<BootstrapHelloV2>(&oversized, &mut budget, limits).is_err());
}

#[test]
fn business_requests_bind_project_instance_and_query_policy() {
    let request = RequestEnvelope::new(
        2,
        RequestId::from_bytes([0x44; 16]),
        project_id(),
        instance_id(),
        QueryPolicyId::from_bytes([0x55; 32]),
        RequestOperation::Search(SearchRequest {
            query: "player controller".to_owned(),
            limit: 25,
        }),
    )
    .unwrap();

    request.validate().unwrap();
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["project_id"], project_id().to_string());
    assert_eq!(value["daemon_instance_id"], instance_id().to_string());
    assert_eq!(
        value["query_policy_id"],
        QueryPolicyId::from_bytes([0x55; 32]).to_string()
    );

    let mut unknown_nested = value;
    unknown_nested["operation"]["request"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<RequestEnvelope>(unknown_nested).is_err());
}

#[test]
fn validated_frame_decode_rejects_semantically_invalid_requests() {
    let request = RequestEnvelope::new(
        2,
        RequestId::from_bytes([0x44; 16]),
        project_id(),
        instance_id(),
        QueryPolicyId::from_bytes([0x55; 32]),
        RequestOperation::Search(SearchRequest {
            query: "player".to_owned(),
            limit: 25,
        }),
    )
    .unwrap();
    let mut value = serde_json::to_value(request).unwrap();
    value["operation"]["request"]["limit"] = serde_json::json!(1_001);
    let encoded = serde_json::to_vec(&value).unwrap();
    let mut frame = Vec::with_capacity(4 + encoded.len());
    frame.extend_from_slice(&u32::try_from(encoded.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(&encoded);

    let mut budget = AssetLoadBudget::default();
    assert!(decode_request_frame(&frame, &mut budget).is_err());
}

#[test]
fn request_framing_validates_before_encoding_and_dispatch() {
    let request = RequestEnvelope::new(
        2,
        RequestId::from_bytes([0x44; 16]),
        project_id(),
        instance_id(),
        QueryPolicyId::from_bytes([0x55; 32]),
        RequestOperation::Search(SearchRequest {
            query: "player".to_owned(),
            limit: 25,
        }),
    )
    .unwrap();
    let frame = encode_request_frame(&request).unwrap();
    let mut budget = AssetLoadBudget::default();
    assert_eq!(decode_request_frame(&frame, &mut budget).unwrap(), request);
}

#[test]
fn frame_encoding_stops_at_the_exact_encoded_byte_limit() {
    let limits = FrameLimits::bootstrap();
    let exact = "x".repeat(limits.max_encoded_bytes() - 2);
    assert_eq!(
        encode_frame(&exact, limits).unwrap().len(),
        limits.max_encoded_bytes() + 4
    );

    let one_over = format!("{exact}x");
    assert!(encode_frame(&one_over, limits).is_err());
}

#[test]
fn business_frame_decode_rejects_noncanonical_json_spellings() {
    let request = RequestEnvelope::new(
        2,
        RequestId::from_bytes([0xaa; 16]),
        project_id(),
        instance_id(),
        QueryPolicyId::from_bytes([0x55; 32]),
        RequestOperation::References(ReferenceRequest::incoming_guid(
            "0123456789abcdef0123456789abcdef",
            None,
            25,
        )),
    )
    .unwrap();
    let canonical = encode_request_frame(&request).unwrap();
    let canonical_json = std::str::from_utf8(&canonical[4..]).unwrap();

    let reordered_value: serde_json::Value = serde_json::from_str(canonical_json).unwrap();
    let reordered = serde_json::to_vec(&reordered_value).unwrap();
    assert_ne!(reordered, &canonical[4..]);
    assert_noncanonical_request(reordered);

    let explicit_null = canonical_json.replace("\"limit\":25}", "\"limit\":25,\"cursor\":null}");
    assert_ne!(explicit_null, canonical_json);
    assert_noncanonical_request(explicit_null.into_bytes());

    let uppercase_id = canonical_json.replace(
        "request-v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "request-v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    );
    assert_ne!(uppercase_id, canonical_json);
    let mut budget = AssetLoadBudget::default();
    assert!(decode_request_frame(&frame_from_json(uppercase_id.as_bytes()), &mut budget,).is_err());

    let response = ResponseEnvelope::error(
        &request,
        ApiError::new(ApiErrorCode::NotReady, "index is booting", true),
    );
    let canonical_response = encode_response_frame(&response, &request).unwrap();
    let duplicate_detail = std::str::from_utf8(&canonical_response[4..])
        .unwrap()
        .replace(
            "\"details\":{}",
            "\"details\":{\"reason\":\"a\",\"reason\":\"b\"}",
        );
    let mut budget = AssetLoadBudget::default();
    assert!(matches!(
        decode_response_frame(
            &frame_from_json(duplicate_detail.as_bytes()),
            &mut budget,
            &request,
        ),
        Err(FramingError::NonCanonicalJson)
    ));
}

#[test]
fn bootstrap_reply_rejects_invalid_versions_and_revisions_during_decode() {
    let wrong_version = serde_json::json!({
        "result": "rejected",
        "bootstrap_version": 1,
        "code": "no_common_revision"
    });
    assert!(serde_json::from_value::<BootstrapReplyV2>(wrong_version).is_err());

    let zero_revision = serde_json::json!({
        "result": "accepted",
        "bootstrap_version": 2,
        "project_id": project_id(),
        "daemon_instance_id": instance_id(),
        "query_policy_id": query_policy_id(),
        "selected_revision": 0
    });
    assert!(serde_json::from_value::<BootstrapReplyV2>(zero_revision).is_err());
}

#[test]
fn bootstrap_reply_must_bind_the_original_project_and_instance() {
    let hello = BootstrapHelloV2::new(
        project_id(),
        instance_id(),
        vec![BUSINESS_PROTOCOL_REVISION],
    )
    .unwrap();
    let wrong_project = serde_json::json!({
        "result": "accepted",
        "bootstrap_version": 2,
        "project_id": ProjectId::from_bytes([0x33; 32]),
        "daemon_instance_id": instance_id(),
        "query_policy_id": query_policy_id(),
        "selected_revision": BUSINESS_PROTOCOL_REVISION
    });
    let reply: BootstrapReplyV2 = serde_json::from_value(wrong_project).unwrap();
    assert!(reply.validate_for(&hello).is_err());
}

#[test]
fn bootstrap_reply_rejects_an_uninitialized_query_policy() {
    let value = serde_json::json!({
        "result": "accepted",
        "bootstrap_version": 2,
        "project_id": project_id(),
        "daemon_instance_id": instance_id(),
        "query_policy_id": QueryPolicyId::from_bytes([0; 32]),
        "selected_revision": BUSINESS_PROTOCOL_REVISION
    });

    assert!(serde_json::from_value::<BootstrapReplyV2>(value).is_err());
}

#[test]
fn identifiers_have_language_neutral_canonical_wire_forms() {
    assert_eq!(
        project_id().to_string(),
        format!("project-v1:{}", "11".repeat(32))
    );
    assert_eq!(
        instance_id().to_string(),
        format!("daemon-v1:{}", "22".repeat(16))
    );

    for malformed in [
        "project-v1:",
        "project-v1:AA",
        "project-v1:zzzz",
        "daemon-v1:2222",
    ] {
        assert!(serde_json::from_value::<ProjectId>(serde_json::json!(malformed)).is_err());
    }
}

fn assert_noncanonical_request(json: Vec<u8>) {
    let mut budget = AssetLoadBudget::default();
    assert!(matches!(
        decode_request_frame(&frame_from_json(&json), &mut budget),
        Err(FramingError::NonCanonicalJson)
    ));
}

fn frame_from_json(json: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + json.len());
    frame.extend_from_slice(&u32::try_from(json.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(json);
    frame
}
