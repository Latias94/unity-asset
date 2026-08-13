use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use unity_asset_search_protocol::{
    BUSINESS_PROTOCOL_REVISION, BootstrapErrorCode, BootstrapHelloV2, BootstrapReplyV2,
    DaemonInstanceId, ProjectId, QueryPolicyId, RequestEnvelope, ResponseEnvelope,
    ValidateContract,
};

const FROZEN_BUSINESS_V1_INVENTORY_SHA256: &str =
    "13cf5971f83e9a608c504582a36c442e79a982c9eb9dbad8d447a41c7694022a";
const FROZEN_BUSINESS_V2_INVENTORY_SHA256: &str =
    "6891e3190d36396e546989a0f55ac97766902aa37289993b5f4709ffa3ccf776";
const FROZEN_BUSINESS_V3_INVENTORY_SHA256: &str =
    "5774a6331cf7f560d389b86bd268639672304d4ad638dd9d8a5a6053b49a9d7a";
const FROZEN_BUSINESS_V4_INVENTORY_SHA256: &str =
    "43a825a10cf984122d4c6fd4f8d6d33e9d9a09cdbce17711924df46fa21b00c7";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureManifest {
    fixture_format: u16,
    protocol_revision: u16,
    frozen_inventories: Vec<FrozenInventoryReference>,
    binding: FixtureBinding,
    valid: Vec<FixtureEntry>,
    invalid: Vec<FixtureEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenInventoryReference {
    business_revision: u16,
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenBusinessInventory {
    inventory_format: u16,
    business_revision: u16,
    files: Vec<FrozenFixture>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenFixture {
    path: String,
    encoded_bytes: usize,
    sha256: String,
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
    assert_eq!(manifest.fixture_format, 3);
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
    ] {
        assert!(
            valid_names.contains(expected),
            "missing shared positive fixture: {expected}"
        );
    }
    assert_frozen_business(&root, &manifest.frozen_inventories);

    let project = ProjectId::from_str(&manifest.binding.project_id).unwrap();
    let instance = DaemonInstanceId::from_str(&manifest.binding.daemon_instance_id).unwrap();
    let query_policy = QueryPolicyId::from_str(&manifest.binding.query_policy_id).unwrap();

    for fixture in &manifest.valid {
        let bytes = read_canonical(&root.join(&fixture.path));
        match fixture.kind.as_str() {
            "bootstrap_hello" => {
                let value: BootstrapHelloV2 = serde_json::from_slice(&bytes).unwrap();
                value.validate().unwrap();
                assert_canonical(fixture, &bytes, &value);
            }
            "bootstrap_reply" => {
                let value: BootstrapReplyV2 = serde_json::from_slice(&bytes).unwrap();
                value.validate().unwrap();
                if fixture.name == "bootstrap rejected" {
                    assert!(matches!(
                        &value,
                        BootstrapReplyV2::Rejected {
                            code: BootstrapErrorCode::NoCommonRevision,
                            ..
                        }
                    ));
                }
                assert_canonical(fixture, &bytes, &value);
            }
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
            "bootstrap_hello" => serde_json::from_slice::<BootstrapHelloV2>(&bytes)
                .and_then(|value| {
                    value.validate().map_err(serde::de::Error::custom)?;
                    Ok(value)
                })
                .unwrap_err()
                .to_string(),
            "bootstrap_reply" => match serde_json::from_slice::<BootstrapReplyV2>(&bytes) {
                Ok(reply) => reply
                    .validate_for(&fixture_hello(&root))
                    .unwrap_err()
                    .to_string(),
                Err(error) => error.to_string(),
            },
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

fn assert_frozen_business(root: &Path, references: &[FrozenInventoryReference]) {
    let expected = [
        (1, FROZEN_BUSINESS_V1_INVENTORY_SHA256),
        (2, FROZEN_BUSINESS_V2_INVENTORY_SHA256),
        (3, FROZEN_BUSINESS_V3_INVENTORY_SHA256),
        (4, FROZEN_BUSINESS_V4_INVENTORY_SHA256),
    ];
    assert_eq!(references.len(), expected.len());

    for (reference, (business_revision, expected_digest)) in references.iter().zip(expected) {
        assert_eq!(reference.business_revision, business_revision);
        assert_eq!(reference.sha256, expected_digest);

        let inventory_bytes = read_canonical(&root.join(&reference.path));
        assert_eq!(
            hex::encode(Sha256::digest(&inventory_bytes)),
            expected_digest,
            "frozen business v{business_revision} inventory changed"
        );
        let inventory: FrozenBusinessInventory = serde_json::from_slice(&inventory_bytes).unwrap();
        assert_eq!(inventory.inventory_format, 1);
        assert_eq!(inventory.business_revision, business_revision);
        assert!(!inventory.files.is_empty());

        let suffix = format!("-v{business_revision}.json");
        let mut previous = None;
        let mut inventoried = BTreeSet::new();
        for fixture in &inventory.files {
            if let Some(previous) = previous {
                assert!(
                    previous < fixture.path.as_str(),
                    "frozen inventory is not sorted"
                );
            }
            previous = Some(fixture.path.as_str());
            assert!(
                fixture.path.ends_with(&suffix)
                    && (fixture.path.starts_with("requests/")
                        || fixture.path.starts_with("responses/")
                        || fixture.path.starts_with("invalid/request-")),
                "unexpected frozen business fixture path: {}",
                fixture.path
            );
            assert!(inventoried.insert(fixture.path.clone()));

            let payload = read_canonical(&root.join(&fixture.path));
            assert_eq!(payload.len(), fixture.encoded_bytes, "{}", fixture.path);
            assert_eq!(
                hex::encode(Sha256::digest(&payload)),
                fixture.sha256,
                "{} changed after business v{business_revision} was frozen",
                fixture.path
            );
        }

        assert_eq!(
            inventoried,
            archived_business_paths(root, business_revision)
        );
    }
}

fn archived_business_paths(root: &Path, business_revision: u16) -> BTreeSet<String> {
    let suffix = format!("-v{business_revision}.json");
    ["requests", "responses", "invalid"]
        .into_iter()
        .flat_map(|directory| {
            fs::read_dir(root.join(directory))
                .unwrap()
                .map(move |entry| (directory, entry.unwrap()))
        })
        .filter_map(|(directory, entry)| {
            let name = entry.file_name().into_string().unwrap();
            let is_archived_business =
                name.ends_with(&suffix) && (directory != "invalid" || name.starts_with("request-"));
            is_archived_business.then(|| format!("{directory}/{name}"))
        })
        .collect()
}

fn fixture_hello(root: &Path) -> BootstrapHelloV2 {
    serde_json::from_slice(&read_canonical(&root.join("bootstrap/hello-v2.json"))).unwrap()
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
