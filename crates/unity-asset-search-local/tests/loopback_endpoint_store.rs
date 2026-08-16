#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use std::fs::OpenOptions;

use unity_asset_search_local::{
    EndpointStoreError, LoopbackEndpointCleanup, LoopbackEndpointPublishError, PrivateRootsV1,
};
use unity_asset_search_protocol::{DaemonInstanceId, ProjectId, QueryPolicyId};

#[test]
fn public_loopback_owner_publishes_after_binding_without_owning_the_listener() {
    let roots = PrivateRootsV1::discover_for_current_context().unwrap();
    let mut project_bytes = rand::random::<[u8; 32]>();
    project_bytes[0] |= 1;
    let namespace = roots
        .runtime()
        .endpoint_namespace(ProjectId::from_bytes(project_bytes))
        .unwrap();
    let cleanup_path = namespace.path().to_path_buf();
    let mut claim = namespace.claim_loopback_endpoint().unwrap();
    let capability = claim.capability().clone();
    let mut published = claim
        .publish(
            DaemonInstanceId::from_bytes([0x22; 16]),
            42_424,
            QueryPolicyId::from_bytes([0x44; 32]),
        )
        .unwrap();

    assert!(published.descriptor().capability().matches(&capability));
    let discovered = namespace.discover_loopback_endpoint().unwrap();
    assert_eq!(discovered.descriptor(), published.descriptor());
    discovered.ensure_unchanged(&namespace).unwrap();
    assert_eq!(
        published.withdraw().unwrap(),
        LoopbackEndpointCleanup::Removed
    );
    assert!(matches!(
        discovered.ensure_unchanged(&namespace),
        Err(EndpointStoreError::EndpointChanged)
    ));
    assert!(matches!(
        namespace.discover_loopback_endpoint(),
        Err(EndpointStoreError::DescriptorMissing)
    ));

    drop(published);
    drop(namespace);
    drop(roots);
    for name in [".daemon-v1.lock", "binding.v1", ".binding-v1.lock"] {
        let result = std::fs::remove_file(cleanup_path.join(name));
        assert!(
            result.is_ok()
                || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        );
    }
    std::fs::remove_dir(cleanup_path).unwrap();
}

#[test]
fn failed_loopback_publication_retains_the_daemon_lease_and_allows_retry() {
    let roots = PrivateRootsV1::discover_for_current_context().unwrap();
    let mut project_bytes = rand::random::<[u8; 32]>();
    project_bytes[0] |= 1;
    let namespace = roots
        .runtime()
        .endpoint_namespace(ProjectId::from_bytes(project_bytes))
        .unwrap();
    let cleanup_path = namespace.path().to_path_buf();
    let mut claim = namespace.claim_loopback_endpoint().unwrap();
    let staging_path = cleanup_path.join(".endpoint-v2.staging");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging_path)
        .unwrap();

    assert!(matches!(
        claim.publish(
            DaemonInstanceId::from_bytes([0x22; 16]),
            42_424,
            QueryPolicyId::from_bytes([0x44; 32]),
        ),
        Err(LoopbackEndpointPublishError::Store(
            EndpointStoreError::PublicationPreCommit { .. }
        ))
    ));
    assert!(matches!(
        namespace.claim_loopback_endpoint(),
        Err(EndpointStoreError::LeaseHeld)
    ));
    std::fs::remove_file(staging_path).unwrap();

    let mut published = claim
        .publish(
            DaemonInstanceId::from_bytes([0x22; 16]),
            42_424,
            QueryPolicyId::from_bytes([0x44; 32]),
        )
        .unwrap();
    assert!(matches!(
        claim.publish(
            DaemonInstanceId::from_bytes([0x33; 16]),
            42_425,
            QueryPolicyId::from_bytes([0x55; 32]),
        ),
        Err(LoopbackEndpointPublishError::AlreadyPublished)
    ));
    assert_eq!(
        published.withdraw().unwrap(),
        LoopbackEndpointCleanup::Removed
    );

    drop(published);
    drop(namespace);
    drop(roots);
    for name in [".daemon-v1.lock", "binding.v1", ".binding-v1.lock"] {
        let result = std::fs::remove_file(cleanup_path.join(name));
        assert!(
            result.is_ok()
                || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        );
    }
    std::fs::remove_dir(cleanup_path).unwrap();
}
