use std::str::FromStr as _;

use unity_asset_search_local::{LocalIdentityParseError, SecurityContextIdV1};

#[test]
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn current_security_context_is_stable_and_round_trips() {
    let first = SecurityContextIdV1::current().unwrap();
    let second = SecurityContextIdV1::current().unwrap();

    assert_eq!(first, second);
    assert_eq!(
        SecurityContextIdV1::from_str(&first.to_string()).unwrap(),
        first
    );
    assert_eq!(
        SecurityContextIdV1::for_process(std::process::id()).unwrap(),
        first
    );
}

#[test]
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn unsupported_platform_reports_a_typed_error() {
    assert!(matches!(
        SecurityContextIdV1::current(),
        Err(unity_asset_search_local::SecurityContextError::UnsupportedPlatform)
    ));
}

#[test]
fn security_context_rejects_noncanonical_and_zero_values() {
    let valid = format!("security-context-v1:{}", "01".repeat(32));
    assert!(SecurityContextIdV1::from_str(&valid).is_ok());

    let uppercase = format!("security-context-v1:{}", "AB".repeat(32));
    assert!(matches!(
        SecurityContextIdV1::from_str(&uppercase),
        Err(LocalIdentityParseError::InvalidEncoding)
    ));
    assert!(matches!(
        SecurityContextIdV1::from_str(&format!("security-context-v1:{}", "00".repeat(32))),
        Err(LocalIdentityParseError::ZeroValue)
    ));
    assert!(matches!(
        SecurityContextIdV1::from_str(&valid.replacen("security-context-v1", "context", 1)),
        Err(LocalIdentityParseError::InvalidPrefix { .. })
    ));
    assert!(matches!(
        SecurityContextIdV1::from_str("security-context-v1:01"),
        Err(LocalIdentityParseError::InvalidLength { .. })
    ));
}

#[test]
fn security_context_rejects_zero_process_id() {
    assert!(matches!(
        SecurityContextIdV1::for_process(0),
        Err(unity_asset_search_local::SecurityContextError::InvalidProcessId)
    ));
}
