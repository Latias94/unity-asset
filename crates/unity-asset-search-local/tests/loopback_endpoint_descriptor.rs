use std::net::{Ipv4Addr, SocketAddrV4};

use unity_asset_search_local::{
    HttpCapability, HttpCapabilityError, LoopbackEndpointDescriptor,
    LoopbackEndpointDescriptorError, MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES,
};
use unity_asset_search_protocol::{
    BUSINESS_PROTOCOL_REVISION, DaemonInstanceId, ProjectId, QueryPolicyId,
};

fn capability(byte: u8) -> HttpCapability {
    HttpCapability::from_bytes([byte; 32]).unwrap()
}

fn descriptor() -> LoopbackEndpointDescriptor {
    LoopbackEndpointDescriptor::new(
        ProjectId::from_bytes([0x11; 32]),
        DaemonInstanceId::from_bytes([0x22; 16]),
        42_424,
        capability(0x33),
        QueryPolicyId::from_bytes([0x44; 32]),
        4_242,
    )
    .unwrap()
}

#[test]
fn capabilities_are_fresh_nonzero_256_bit_secrets() {
    let first = HttpCapability::generate().unwrap();
    let second = HttpCapability::generate().unwrap();
    let first_encoded = first.encode_hex();

    assert!(!first.matches(&second));
    assert_eq!(first_encoded.len(), 64);
    assert!(first_encoded.iter().any(|byte| *byte != b'0'));
    assert!(
        first_encoded
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    assert!(matches!(
        HttpCapability::from_bytes([0; 32]),
        Err(HttpCapabilityError::ZeroValue)
    ));
}

#[test]
fn capability_debug_output_is_redacted() {
    let capability = capability(0xab);
    let descriptor = LoopbackEndpointDescriptor::new(
        ProjectId::from_bytes([0x11; 32]),
        DaemonInstanceId::from_bytes([0x22; 16]),
        42_424,
        capability.clone(),
        QueryPolicyId::from_bytes([0x44; 32]),
        4_242,
    )
    .unwrap();

    assert_eq!(format!("{capability:?}"), "HttpCapability(<redacted>)");
    assert!(!format!("{descriptor:?}").contains(&"ab".repeat(32)));
}

#[test]
fn capability_matching_covers_equal_and_mismatched_fixed_size_candidates() {
    let expected = capability(0x55);
    let matching = expected.clone();
    let mut first_byte_differs = [0x55; 32];
    first_byte_differs[0] = 0x54;
    let mut last_byte_differs = [0x55; 32];
    last_byte_differs[31] = 0x54;

    assert!(expected.matches(&matching));
    assert_eq!(expected, matching);
    assert!(!expected.matches(&HttpCapability::from_bytes(first_byte_differs).unwrap()));
    assert!(!expected.matches(&HttpCapability::from_bytes(last_byte_differs).unwrap()));
}

#[test]
fn capability_wire_parsing_rejects_noncanonical_and_wrong_length_values() {
    assert_eq!(
        HttpCapability::from_hex(&"11".repeat(32)).unwrap(),
        capability(0x11)
    );
    assert!(matches!(
        HttpCapability::from_hex(&"AA".repeat(32)),
        Err(HttpCapabilityError::InvalidEncoding)
    ));
    assert!(matches!(
        HttpCapability::from_hex("11"),
        Err(HttpCapabilityError::InvalidLength {
            expected: 64,
            actual: 2,
        })
    ));
}

#[test]
fn loopback_descriptor_has_one_exact_canonical_wire_representation() {
    let descriptor = descriptor();
    let encoded = descriptor.encode_json().unwrap();
    let expected = concat!(
        "{\"descriptor_version\":2,",
        "\"project_id\":\"project-v1:1111111111111111111111111111111111111111111111111111111111111111\",",
        "\"daemon_instance_id\":\"daemon-v1:22222222222222222222222222222222\",",
        "\"port\":42424,",
        "\"capability\":\"3333333333333333333333333333333333333333333333333333333333333333\",",
        "\"business_protocol_revision\":5,",
        "\"query_policy_id\":\"query-policy-v1:4444444444444444444444444444444444444444444444444444444444444444\",",
        "\"server_pid\":4242}"
    );

    assert_eq!(encoded, expected.as_bytes());
    assert_eq!(
        LoopbackEndpointDescriptor::decode_json(&encoded).unwrap(),
        descriptor
    );
    assert_eq!(
        descriptor.socket_addr(),
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, 42_424)
    );
    assert_eq!(
        descriptor.business_protocol_revision(),
        BUSINESS_PROTOCOL_REVISION
    );
    descriptor
        .validate_binding(
            ProjectId::from_bytes([0x11; 32]),
            QueryPolicyId::from_bytes([0x44; 32]),
        )
        .unwrap();
    assert!(matches!(
        descriptor.validate_binding(
            ProjectId::from_bytes([0x99; 32]),
            QueryPolicyId::from_bytes([0x44; 32]),
        ),
        Err(LoopbackEndpointDescriptorError::BindingMismatch {
            field: "project_id"
        })
    ));
    assert!(matches!(
        descriptor.validate_binding(
            ProjectId::from_bytes([0x11; 32]),
            QueryPolicyId::from_bytes([0x99; 32]),
        ),
        Err(LoopbackEndpointDescriptorError::BindingMismatch {
            field: "query_policy_id"
        })
    ));
}

#[test]
fn loopback_descriptor_rejects_v1_unknown_trailing_and_noncanonical_json() {
    let v1 = concat!(
        "{\"descriptor_version\":1,",
        "\"project_id\":\"project-v1:1111111111111111111111111111111111111111111111111111111111111111\",",
        "\"daemon_instance_id\":\"daemon-v1:22222222222222222222222222222222\",",
        "\"server_pid\":4242,",
        "\"process_start_identity\":\"process-start-v1:3333333333333333333333333333333333333333333333333333333333333333\",",
        "\"security_context_id\":\"security-context-v1:5555555555555555555555555555555555555555555555555555555555555555\",",
        "\"bootstrap_version\":1}"
    );
    assert!(matches!(
        LoopbackEndpointDescriptor::decode_json(v1.as_bytes()),
        Err(LoopbackEndpointDescriptorError::UnsupportedDescriptorVersion { actual: 1 })
    ));

    let encoded = descriptor().encode_json().unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    value["unexpected"] = serde_json::json!(true);
    assert!(matches!(
        LoopbackEndpointDescriptor::decode_json(&serde_json::to_vec(&value).unwrap()),
        Err(LoopbackEndpointDescriptorError::Json(_))
    ));

    let mut trailing = encoded.clone();
    trailing.extend_from_slice(br#"{}"#);
    assert!(matches!(
        LoopbackEndpointDescriptor::decode_json(&trailing),
        Err(LoopbackEndpointDescriptorError::Json(_))
    ));

    let mut spaced = encoded;
    spaced.push(b' ');
    assert!(matches!(
        LoopbackEndpointDescriptor::decode_json(&spaced),
        Err(LoopbackEndpointDescriptorError::NonCanonicalJson)
    ));
}

#[test]
fn loopback_descriptor_rejects_zero_fields_and_wrong_business_revision() {
    assert!(matches!(
        LoopbackEndpointDescriptor::new(
            ProjectId::from_bytes([0x11; 32]),
            DaemonInstanceId::from_bytes([0x22; 16]),
            0,
            capability(0x33),
            QueryPolicyId::from_bytes([0x44; 32]),
            4_242,
        ),
        Err(LoopbackEndpointDescriptorError::ZeroField { field: "port" })
    ));

    assert!(matches!(
        LoopbackEndpointDescriptor::new(
            ProjectId::from_bytes([0x11; 32]),
            DaemonInstanceId::from_bytes([0x22; 16]),
            42_424,
            capability(0x33),
            QueryPolicyId::from_bytes([0x44; 32]),
            0,
        ),
        Err(LoopbackEndpointDescriptorError::ZeroField {
            field: "server_pid"
        })
    ));

    let encoded = descriptor().encode_json().unwrap();
    for (field, expected_error_field) in [
        ("project_id", "project_id"),
        ("daemon_instance_id", "daemon_instance_id"),
        ("query_policy_id", "query_policy_id"),
    ] {
        let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        let current = value[field].as_str().unwrap();
        let prefix = current.split_once(':').unwrap().0;
        let payload_len = current.len() - prefix.len() - 1;
        value[field] = serde_json::json!(format!("{prefix}:{}", "0".repeat(payload_len)));
        assert!(matches!(
            LoopbackEndpointDescriptor::decode_json(&serde_json::to_vec(&value).unwrap()),
            Err(LoopbackEndpointDescriptorError::ZeroField { field })
                if field == expected_error_field
        ));
    }

    let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    value["port"] = serde_json::json!(0);
    assert!(matches!(
        LoopbackEndpointDescriptor::decode_json(&serde_json::to_vec(&value).unwrap()),
        Err(LoopbackEndpointDescriptorError::ZeroField { field: "port" })
    ));

    let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    value["business_protocol_revision"] = serde_json::json!(BUSINESS_PROTOCOL_REVISION + 1);
    assert!(matches!(
        LoopbackEndpointDescriptor::decode_json(&serde_json::to_vec(&value).unwrap()),
        Err(LoopbackEndpointDescriptorError::UnsupportedBusinessProtocolRevision { .. })
    ));
}

#[test]
fn loopback_descriptor_enforces_the_exact_encoded_size_budget() {
    let mut exact = descriptor().encode_json().unwrap();
    exact.resize(MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES, b' ');
    assert!(matches!(
        LoopbackEndpointDescriptor::decode_json(&exact),
        Err(LoopbackEndpointDescriptorError::NonCanonicalJson)
    ));

    let oversized = vec![b' '; MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES + 1];
    assert!(matches!(
        LoopbackEndpointDescriptor::decode_json(&oversized),
        Err(LoopbackEndpointDescriptorError::EncodedSizeLimit {
            actual,
            maximum: MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES,
        }) if actual == MAX_LOOPBACK_ENDPOINT_DESCRIPTOR_BYTES + 1
    ));
}
