use unity_asset_search_local::{ProcessIdentityError, ProcessIdentityV1, SecurityContextIdV1};

#[test]
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn current_process_identity_is_stable_and_matches_direct_inspection() {
    let current = ProcessIdentityV1::current().unwrap();
    let inspected = ProcessIdentityV1::inspect(std::process::id()).unwrap();

    assert_eq!(current, inspected);
    assert_eq!(current.process_id(), std::process::id());
    assert_eq!(
        current.security_context_id(),
        SecurityContextIdV1::current().unwrap()
    );
}

#[test]
fn zero_process_id_is_rejected_before_platform_inspection() {
    assert!(matches!(
        ProcessIdentityV1::inspect(0),
        Err(ProcessIdentityError::InvalidProcessId)
    ));
}
