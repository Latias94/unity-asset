#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use std::time::{Duration, Instant};
use std::{sync::Arc, sync::Barrier, thread};

use unity_asset_search_local::{
    EndpointClaimError, EndpointCleanupV1, EndpointStoreError, EndpointTransportError,
    FrameReadTimeoutsV1, PrivateRootsV1, generate_daemon_instance_id,
};
use unity_asset_search_protocol::{DaemonInstanceId, FrameLimits, ProjectId};

const BINDING_STAGING_FILE: &str = ".binding-v1.staging";
const BINDING_LOCK_FILE: &str = ".binding-v1.lock";

#[tokio::test]
async fn same_principal_connects_to_the_published_process() {
    let roots = PrivateRootsV1::discover_for_current_context().unwrap();
    let mut project_bytes = rand::random::<[u8; 32]>();
    project_bytes[0] |= 1;
    let namespace = roots
        .runtime()
        .endpoint_namespace(ProjectId::from_bytes(project_bytes))
        .unwrap();
    let cleanup_path = namespace.path().to_path_buf();
    let mut claim = namespace.claim_daemon_endpoint().unwrap();
    assert_eq!(claim.stale_cleanup(), EndpointCleanupV1::AlreadyAbsent);
    let instance = generate_daemon_instance_id().unwrap();
    let mut endpoint = claim.publish(instance).unwrap();
    assert!(matches!(
        claim.publish(instance),
        Err(EndpointClaimError::AlreadyPublished)
    ));
    let discovered = namespace.discover_endpoint().unwrap();

    let (accepted, connected) = tokio::join!(
        endpoint.accept_verified(),
        discovered.connect_verified(&namespace, Instant::now() + Duration::from_secs(5))
    );
    let mut accepted = accepted.unwrap();
    let mut connected = connected.unwrap();
    assert_eq!(accepted.peer_identity().process_id(), std::process::id());
    assert_eq!(connected.peer_identity().process_id(), std::process::id());

    for client_frame in [
        [0, 0, 0, 4, b'p', b'i', b'n', b'g'],
        [0, 0, 0, 4, b'n', b'e', b'x', b't'],
    ] {
        connected
            .write_frame(
                &client_frame,
                FrameLimits::bootstrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(
            accepted
                .read_frame(
                    FrameLimits::bootstrap(),
                    FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
                )
                .await
                .unwrap()
                .unwrap(),
            client_frame
        );
    }

    for server_frame in [
        [0, 0, 0, 4, b'p', b'o', b'n', b'g'],
        [0, 0, 0, 4, b'd', b'o', b'n', b'e'],
    ] {
        accepted
            .write_frame(
                &server_frame,
                FrameLimits::bootstrap(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(
            connected
                .read_frame(
                    FrameLimits::bootstrap(),
                    FrameReadTimeoutsV1::uniform(Duration::from_secs(5)),
                )
                .await
                .unwrap()
                .unwrap(),
            server_frame
        );
    }

    drop(accepted);
    drop(connected);
    assert_eq!(endpoint.withdraw().unwrap(), EndpointCleanupV1::Removed);
    drop(endpoint);
    drop(namespace);
    drop(roots);

    for name in ["binding.v1", BINDING_LOCK_FILE, ".daemon-v1.lock"] {
        let result = std::fs::remove_file(cleanup_path.join(name));
        assert!(
            result.is_ok()
                || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        );
    }
    std::fs::remove_dir(cleanup_path).unwrap();
}

#[tokio::test]
async fn a_previously_discovered_endpoint_becoming_absent_is_a_generation_change() {
    let roots = PrivateRootsV1::discover_for_current_context().unwrap();
    let mut project_bytes = rand::random::<[u8; 32]>();
    project_bytes[0] |= 1;
    let namespace = roots
        .runtime()
        .endpoint_namespace(ProjectId::from_bytes(project_bytes))
        .unwrap();
    let cleanup_path = namespace.path().to_path_buf();
    let mut claim = namespace.claim_daemon_endpoint().unwrap();
    let mut endpoint = claim
        .publish(generate_daemon_instance_id().unwrap())
        .unwrap();
    let discovered = namespace.discover_endpoint().unwrap();

    assert_eq!(endpoint.withdraw().unwrap(), EndpointCleanupV1::Removed);
    assert!(matches!(
        discovered.ensure_unchanged(&namespace),
        Err(EndpointStoreError::EndpointChanged)
    ));

    drop(endpoint);
    drop(claim);
    drop(namespace);
    drop(roots);
    for name in ["binding.v1", BINDING_LOCK_FILE, ".daemon-v1.lock"] {
        let result = std::fs::remove_file(cleanup_path.join(name));
        assert!(
            result.is_ok()
                || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        );
    }
    std::fs::remove_dir(cleanup_path).unwrap();
}

#[test]
fn failed_publish_retains_daemon_lease_until_claim_is_dropped() {
    let roots = PrivateRootsV1::discover_for_current_context().unwrap();
    let mut project_bytes = rand::random::<[u8; 32]>();
    project_bytes[0] |= 1;
    let namespace = roots
        .runtime()
        .endpoint_namespace(ProjectId::from_bytes(project_bytes))
        .unwrap();
    let cleanup_path = namespace.path().to_path_buf();
    let mut claim = namespace.claim_daemon_endpoint().unwrap();

    assert!(matches!(
        claim.publish(DaemonInstanceId::from_bytes([0; 16])),
        Err(EndpointClaimError::Transport(
            EndpointTransportError::ZeroDaemonInstanceId
        ))
    ));
    assert!(matches!(
        namespace.claim_daemon_endpoint(),
        Err(EndpointStoreError::LeaseHeld)
    ));

    drop(claim);
    let replacement_claim = namespace.claim_daemon_endpoint().unwrap();
    drop(replacement_claim);
    drop(namespace);
    drop(roots);

    for name in ["binding.v1", BINDING_LOCK_FILE, ".daemon-v1.lock"] {
        let result = std::fs::remove_file(cleanup_path.join(name));
        assert!(
            result.is_ok()
                || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        );
    }
    std::fs::remove_dir(cleanup_path).unwrap();
}

#[test]
fn concurrent_namespace_binding_never_exposes_partial_json() {
    const CONTENDERS: usize = 8;

    let mut project_bytes = rand::random::<[u8; 32]>();
    project_bytes[0] |= 1;
    let project_id = ProjectId::from_bytes(project_bytes);
    let barrier = Arc::new(Barrier::new(CONTENDERS));
    let contenders = (0..CONTENDERS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let roots = PrivateRootsV1::discover_for_current_context().unwrap();
                barrier.wait();
                roots
                    .runtime()
                    .endpoint_namespace(project_id)
                    .map(|namespace| namespace.path().to_path_buf())
            })
        })
        .collect::<Vec<_>>();

    let paths = contenders
        .into_iter()
        .map(|contender| contender.join().unwrap().unwrap())
        .collect::<Vec<_>>();
    assert!(paths.iter().all(|path| path == &paths[0]));

    let binding = std::fs::read(paths[0].join("binding.v1")).unwrap();
    let decoded: serde_json::Value = serde_json::from_slice(&binding).unwrap();
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), binding);
    assert!(!paths[0].join(BINDING_STAGING_FILE).exists());

    std::fs::remove_file(paths[0].join("binding.v1")).unwrap();
    std::fs::remove_file(paths[0].join(BINDING_LOCK_FILE)).unwrap();
    std::fs::remove_dir(&paths[0]).unwrap();
}
