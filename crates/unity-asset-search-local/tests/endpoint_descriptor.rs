use unity_asset_search_local::{
    EndpointDescriptorError, EndpointDescriptorV1, ExecutableFileIdentityV1,
    ProcessStartIdentityV1, SecurityContextIdV1,
};
use unity_asset_search_protocol::{BOOTSTRAP_VERSION, DaemonInstanceId, ProjectId};

fn descriptor() -> EndpointDescriptorV1 {
    EndpointDescriptorV1::new(
        ProjectId::from_bytes([0x11; 32]),
        DaemonInstanceId::from_bytes([0x22; 16]),
        4242,
        ProcessStartIdentityV1::from_bytes([0x33; 32]).unwrap(),
        ExecutableFileIdentityV1::from_bytes([0x44; 32]).unwrap(),
        SecurityContextIdV1::from_bytes([0x55; 32]).unwrap(),
    )
    .unwrap()
}

#[test]
fn endpoint_descriptor_round_trips_canonical_json() {
    let descriptor = descriptor();
    let encoded = descriptor.encode_json().unwrap();
    let decoded = EndpointDescriptorV1::decode_json(&encoded).unwrap();

    assert_eq!(decoded, descriptor);
    assert_eq!(decoded.encode_json().unwrap(), encoded);
    assert_eq!(decoded.bootstrap_version(), BOOTSTRAP_VERSION);
}

#[test]
fn endpoint_descriptor_rejects_unknown_fields_and_trailing_json() {
    let encoded = descriptor().encode_json().unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    value["unexpected"] = serde_json::json!(true);
    let unknown = serde_json::to_vec(&value).unwrap();
    assert!(matches!(
        EndpointDescriptorV1::decode_json(&unknown),
        Err(EndpointDescriptorError::Json(_))
    ));

    let mut trailing = encoded;
    trailing.extend_from_slice(br#"{}"#);
    assert!(matches!(
        EndpointDescriptorV1::decode_json(&trailing),
        Err(EndpointDescriptorError::Json(_))
    ));
}

#[test]
fn endpoint_descriptor_rejects_invalid_versions_and_zero_fields() {
    let encoded = descriptor().encode_json().unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    value["descriptor_version"] = serde_json::json!(2);
    assert!(matches!(
        EndpointDescriptorV1::decode_json(&serde_json::to_vec(&value).unwrap()),
        Err(EndpointDescriptorError::UnsupportedDescriptorVersion { actual: 2 })
    ));

    value["descriptor_version"] = serde_json::json!(1);
    value["server_pid"] = serde_json::json!(0);
    assert!(matches!(
        EndpointDescriptorV1::decode_json(&serde_json::to_vec(&value).unwrap()),
        Err(EndpointDescriptorError::ZeroField {
            field: "server_pid"
        })
    ));

    value["server_pid"] = serde_json::json!(4242);
    value["bootstrap_version"] = serde_json::json!(BOOTSTRAP_VERSION + 1);
    assert!(matches!(
        EndpointDescriptorV1::decode_json(&serde_json::to_vec(&value).unwrap()),
        Err(EndpointDescriptorError::UnsupportedBootstrapVersion { .. })
    ));

    assert!(matches!(
        EndpointDescriptorV1::new(
            ProjectId::from_bytes([0x11; 32]),
            DaemonInstanceId::from_bytes([0; 16]),
            4242,
            ProcessStartIdentityV1::from_bytes([0x33; 32]).unwrap(),
            ExecutableFileIdentityV1::from_bytes([0x44; 32]).unwrap(),
            SecurityContextIdV1::from_bytes([0x55; 32]).unwrap(),
        ),
        Err(EndpointDescriptorError::ZeroField {
            field: "daemon_instance_id"
        })
    ));

    assert!(matches!(
        EndpointDescriptorV1::new(
            ProjectId::from_bytes([0; 32]),
            DaemonInstanceId::from_bytes([0x22; 16]),
            4242,
            ProcessStartIdentityV1::from_bytes([0x33; 32]).unwrap(),
            ExecutableFileIdentityV1::from_bytes([0x44; 32]).unwrap(),
            SecurityContextIdV1::from_bytes([0x55; 32]).unwrap(),
        ),
        Err(EndpointDescriptorError::ZeroField {
            field: "project_id"
        })
    ));
}

#[test]
fn endpoint_descriptor_rejects_noncanonical_local_identities() {
    let encoded = String::from_utf8(descriptor().encode_json().unwrap()).unwrap();
    let uppercase = encoded.replace(&"33".repeat(32), &"AA".repeat(32));
    assert!(matches!(
        EndpointDescriptorV1::decode_json(uppercase.as_bytes()),
        Err(EndpointDescriptorError::Json(_))
    ));

    let zero = encoded.replace(&"44".repeat(32), &"00".repeat(32));
    assert!(matches!(
        EndpointDescriptorV1::decode_json(zero.as_bytes()),
        Err(EndpointDescriptorError::Json(_))
    ));
}

#[test]
fn endpoint_descriptor_validates_expected_binding() {
    let descriptor = descriptor();
    descriptor
        .validate_binding(
            ProjectId::from_bytes([0x11; 32]),
            SecurityContextIdV1::from_bytes([0x55; 32]).unwrap(),
        )
        .unwrap();

    assert!(matches!(
        descriptor.validate_binding(
            ProjectId::from_bytes([0x99; 32]),
            SecurityContextIdV1::from_bytes([0x55; 32]).unwrap(),
        ),
        Err(EndpointDescriptorError::BindingMismatch {
            field: "project_id"
        })
    ));
    assert!(matches!(
        descriptor.validate_binding(
            ProjectId::from_bytes([0x11; 32]),
            SecurityContextIdV1::from_bytes([0x99; 32]).unwrap(),
        ),
        Err(EndpointDescriptorError::BindingMismatch {
            field: "security_context_id"
        })
    ));
}

#[test]
fn endpoint_descriptor_enforces_encoded_size_before_parsing() {
    let mut exact = descriptor().encode_json().unwrap();
    exact.resize(
        unity_asset_search_local::MAX_ENDPOINT_DESCRIPTOR_BYTES,
        b' ',
    );
    assert!(EndpointDescriptorV1::decode_json(&exact).is_ok());

    let oversized = vec![b' '; unity_asset_search_local::MAX_ENDPOINT_DESCRIPTOR_BYTES + 1];
    assert!(matches!(
        EndpointDescriptorV1::decode_json(&oversized),
        Err(EndpointDescriptorError::EncodedSizeLimit { .. })
    ));
}
