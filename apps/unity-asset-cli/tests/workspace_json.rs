use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use unity_asset::workspace::{
    GenericMutation, MUTATION_PLAN_VERSION, MutationPlan, MutationValue, ObjectGuard,
    SourceExpectation,
};
use unity_asset::{
    DigestV1, ObjectAddress, SourceFingerprint, SourceKind, SourceLocator, WorkspaceId,
    WorkspaceRevision,
};

fn run(arguments: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unity-asset"))
        .args(arguments)
        .output()
        .expect("the built unity-asset binary must start")
}

fn validate(path: &Path) -> Output {
    run(&[
        OsStr::new("workspace"),
        OsStr::new("plan"),
        OsStr::new("validate"),
        OsStr::new("--plan"),
        path.as_os_str(),
    ])
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.stderr.is_empty());
}

fn assert_structured_error_with_code(output: &Output, code: &str, message: &str) -> Value {
    assert!(
        !output.status.success(),
        "invalid plan unexpectedly succeeded"
    );
    assert!(
        output.stdout.is_empty(),
        "failed command wrote stdout:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
    let report: Value =
        serde_json::from_slice(&output.stderr).expect("CLI error must be one JSON document");
    assert_eq!(report["contract"], "unity_asset.cli_error");
    assert_eq!(report["version"], 2);
    assert_eq!(report["status"], "error");
    assert_eq!(report["code"], code);
    assert_eq!(report["message"], message);
    assert!(
        !report["causes"].as_array().unwrap().is_empty(),
        "structured error must retain its cause chain"
    );
    report
}

fn assert_structured_error(output: &Output) -> Value {
    assert_structured_error_with_code(
        output,
        "CLI_CONTRACT_INVALID",
        "structured input contract is invalid",
    )
}

fn canonical_plan() -> Vec<u8> {
    let locator = SourceLocator::path("scene.prefab").unwrap();
    let source = SourceExpectation::new(
        locator.clone(),
        SourceFingerprint::from_bytes(SourceKind::Yaml, b"workspace-json-test"),
    );
    let target = ObjectAddress::yaml(locator, "1001".parse().unwrap()).unwrap();
    let plan = MutationPlan::new(
        WorkspaceId::from_u128(1).unwrap(),
        WorkspaceRevision::new(DigestV1::hash_bytes(b"base-revision")),
        vec![source],
        Vec::new(),
        vec![GenericMutation::SchemaReplace {
            target,
            guard: ObjectGuard::new(
                DigestV1::hash_bytes(b"schema"),
                DigestV1::hash_bytes(b"value"),
            ),
            replacement: MutationValue::signed(7),
        }],
    )
    .expect("test plan must satisfy the public mutation contract");
    plan.canonical_json()
        .expect("test plan must have canonical JSON")
}

#[test]
fn plan_validate_is_a_canonical_round_trip() {
    let temp = tempfile::tempdir().expect("temporary plan directory must be available");
    let input = temp.path().join("plan.json");
    let canonical = canonical_plan();
    fs::write(&input, &canonical).unwrap();

    let first = validate(&input);
    assert_success(&first);
    let mut expected = canonical;
    expected.push(b'\n');
    assert_eq!(first.stdout, expected);

    let validated = temp.path().join("validated.json");
    fs::write(&validated, &first.stdout).unwrap();
    let second = validate(&validated);
    assert_success(&second);
    assert_eq!(second.stdout, first.stdout);

    let plan: Value =
        serde_json::from_slice(&second.stdout).expect("validated plan must remain JSON");
    assert_eq!(plan["version"], MUTATION_PLAN_VERSION);
    assert_eq!(plan["operations"][0]["ordinal"], 0);
    assert_eq!(plan["operations"][0]["action"]["kind"], "schema_replace");
}

#[test]
fn plan_validate_reports_invalid_json_as_a_structured_error() {
    let temp = tempfile::tempdir().expect("temporary plan directory must be available");
    let path = temp.path().join("invalid.json");
    fs::write(&path, br#"{"version":"#).unwrap();

    let report = assert_structured_error(&validate(&path));
    let causes = serde_json::to_string(&report["causes"]).unwrap();
    assert!(causes.contains("EOF"));
}

#[test]
fn plan_validate_reports_unknown_versions_as_a_structured_error() {
    let temp = tempfile::tempdir().expect("temporary plan directory must be available");
    let path = temp.path().join("future.json");
    let mut future: Value = serde_json::from_slice(&canonical_plan()).unwrap();
    future["version"] = Value::from(255);
    fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();

    let report = assert_structured_error(&validate(&path));
    assert_eq!(report["code"], "CLI_CONTRACT_INVALID");
}

#[test]
fn oversized_small_contracts_report_a_typed_budget_error() {
    let temp = tempfile::tempdir().expect("temporary contract directory must be available");
    let path = temp.path().join("oversized-locator.json");
    fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();

    let output = run(&[
        OsStr::new("workspace"),
        OsStr::new("recover"),
        OsStr::new("resume"),
        OsStr::new("--locator-json"),
        path.as_os_str(),
    ]);
    let report = assert_structured_error_with_code(
        &output,
        "CLI_CONTRACT_BUDGET_EXCEEDED",
        "structured input contract exceeded its resource budget",
    );
    assert_eq!(
        report["details"]["contract"],
        "unity_asset.recovery_locator"
    );
    assert_eq!(report["details"]["resource"], "encoded_bytes");
    assert_eq!(report["details"]["limit"], 1024 * 1024);
}

#[test]
fn small_contracts_reject_a_trailing_json_document() {
    let temp = tempfile::tempdir().expect("temporary contract directory must be available");
    let path = temp.path().join("object-address-with-trailing-json.json");
    let address = ObjectAddress::yaml(
        SourceLocator::path("scene.prefab").unwrap(),
        "1001".parse().unwrap(),
    )
    .unwrap();
    let mut encoded = serde_json::to_vec(&address).unwrap();
    encoded.extend_from_slice(b"\nnull");
    fs::write(&path, encoded).unwrap();

    let output = run(&[
        OsStr::new("workspace"),
        OsStr::new("inspect"),
        OsStr::new("object"),
        OsStr::new("--input"),
        temp.path().as_os_str(),
        OsStr::new("--address-json"),
        path.as_os_str(),
    ]);
    assert_structured_error(&output);
}

#[test]
fn small_contracts_report_the_contract_specific_structure_limit() {
    let temp = tempfile::tempdir().expect("temporary contract directory must be available");
    let path = temp.path().join("wide-recovery-locator.json");
    fs::write(&path, serde_json::to_vec(&vec![(); 64]).unwrap()).unwrap();

    let output = run(&[
        OsStr::new("workspace"),
        OsStr::new("recover"),
        OsStr::new("resume"),
        OsStr::new("--locator-json"),
        path.as_os_str(),
    ]);
    let report = assert_structured_error_with_code(
        &output,
        "CLI_CONTRACT_BUDGET_EXCEEDED",
        "structured input contract exceeded its resource budget",
    );
    assert_eq!(
        report["details"]["contract"],
        "unity_asset.recovery_locator"
    );
    assert_eq!(report["details"]["resource"], "entries");
    assert_eq!(report["details"]["limit"], 64);
}
