use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;
use unity_asset_search_protocol::{
    BUSINESS_PROTOCOL_REVISION, DaemonInstanceId, ProjectId, QueryPolicyId, RequestEnvelope,
    ResponseEnvelope,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureManifest {
    fixture_format: u16,
    protocol_revision: u16,
    binding: FixtureBinding,
    valid: Vec<FixtureEntry>,
    invalid: Vec<FixtureEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureBinding {
    project_id: String,
    daemon_instance_id: String,
    query_policy_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureEntry {
    name: String,
    kind: String,
    path: String,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    expected_error: Option<String>,
}

#[test]
fn rust_and_csharp_share_canonical_nonempty_protocol_fixtures() {
    let root = fixture_root();
    let manifest: FixtureManifest =
        serde_json::from_slice(&read_nonempty(&root.join("manifest.json"))).unwrap();
    assert_eq!(manifest.fixture_format, 1);
    assert_eq!(manifest.protocol_revision, BUSINESS_PROTOCOL_REVISION);
    assert!(!manifest.valid.is_empty());
    assert!(!manifest.invalid.is_empty());
    let valid_names = manifest
        .valid
        .iter()
        .map(|fixture| fixture.name.as_str())
        .collect::<BTreeSet<_>>();
    for expected in [
        "unanchored YAML document references response",
        "semantics-stale status response",
        "configuration-stale status response",
        "recovery-required status response",
        "process-failure status response",
    ] {
        assert!(
            valid_names.contains(expected),
            "missing shared positive fixture: {expected}"
        );
    }
    let project = ProjectId::from_str(&manifest.binding.project_id).unwrap();
    let instance = DaemonInstanceId::from_str(&manifest.binding.daemon_instance_id).unwrap();
    let query_policy = QueryPolicyId::from_str(&manifest.binding.query_policy_id).unwrap();

    for fixture in &manifest.valid {
        let bytes = read_canonical(&root.join(&fixture.path));
        match fixture.kind.as_str() {
            "request" => {
                let value: RequestEnvelope = serde_json::from_slice(&bytes).unwrap();
                value
                    .validate_binding(project, instance, query_policy)
                    .unwrap();
                assert_eq!(
                    fixture.operation.as_deref(),
                    Some(operation_name(value.operation().kind()))
                );
                assert_canonical(fixture, &bytes, &value);
            }
            "response" => {
                let request_path = fixture.request.as_ref().expect("response request fixture");
                let request: RequestEnvelope =
                    serde_json::from_slice(&read_canonical(&root.join(request_path))).unwrap();
                let value: ResponseEnvelope = serde_json::from_slice(&bytes).unwrap();
                value.validate_for(&request).unwrap();
                assert_canonical(fixture, &bytes, &value);
            }
            other => panic!("unknown fixture kind {other:?}"),
        }
    }

    for fixture in &manifest.invalid {
        let expected = fixture.expected_error.as_deref().unwrap();
        let bytes = read_canonical(&root.join(&fixture.path));
        let error = match fixture.kind.as_str() {
            "request" => match serde_json::from_slice::<RequestEnvelope>(&bytes) {
                Ok(request) => request
                    .validate_binding(project, instance, query_policy)
                    .unwrap_err()
                    .to_string(),
                Err(error) => error.to_string(),
            },
            other => panic!("unknown invalid fixture kind {other:?}"),
        };
        assert!(
            error
                .to_ascii_lowercase()
                .contains(&expected.to_ascii_lowercase()),
            "{}: rejection {error:?} did not identify {expected:?}",
            fixture.name
        );
    }
}

fn assert_canonical<T: serde::Serialize>(fixture: &FixtureEntry, bytes: &[u8], value: &T) {
    assert_eq!(
        serde_json::to_vec(value).unwrap(),
        bytes,
        "fixture is not canonical: {}",
        fixture.name
    );
}

fn operation_name(kind: unity_asset_search_protocol::OperationKind) -> &'static str {
    use unity_asset_search_protocol::OperationKind;

    match kind {
        OperationKind::Capabilities => "capabilities",
        OperationKind::Status => "status",
        OperationKind::Search => "search",
        OperationKind::Suggest => "suggest",
        OperationKind::References => "references",
        OperationKind::ReindexAdmit => "reindex_admit",
        OperationKind::ReindexStatus => "reindex_status",
        OperationKind::ReindexWait => "reindex_wait",
        OperationKind::ReindexCancel => "reindex_cancel",
        OperationKind::Shutdown => "shutdown",
    }
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../integration/search-protocol/fixtures")
}

fn read_nonempty(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert!(bytes.iter().any(|byte| !byte.is_ascii_whitespace()));
    bytes
}

fn read_canonical(path: &Path) -> Vec<u8> {
    let mut bytes = read_nonempty(path);
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        bytes.pop();
    }
    bytes
}
