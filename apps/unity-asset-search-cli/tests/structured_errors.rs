use std::fs;
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

fn run_cli(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unity-asset-search-cli"))
        .args(arguments)
        .output()
        .expect("run search CLI")
}

fn run_cli_with_stdin(arguments: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_unity-asset-search-cli"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn search CLI");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin)
        .expect("write search CLI stdin");
    child.wait_with_output().expect("wait for search CLI")
}

fn assert_failure(output: &Output, category: &str, exit_code: i32) -> Value {
    assert_eq!(output.status.code(), Some(exit_code));
    assert!(output.stdout.is_empty(), "errors must not write stdout");
    let document: Value =
        serde_json::from_slice(&output.stderr).expect("stderr must contain one JSON document");
    assert_eq!(document["cli_contract_version"], 1);
    assert_eq!(document["category"], category);
    assert_eq!(
        document["error"]["details"]["source"],
        "unity_asset_search_cli"
    );
    assert_eq!(
        output.stderr.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "stderr must contain exactly one newline-terminated document"
    );
    document
}

fn unity_project() -> TempDir {
    let root = tempfile::tempdir().expect("temporary Unity project");
    fs::create_dir(root.path().join("Assets")).expect("Assets marker");
    fs::create_dir(root.path().join("ProjectSettings")).expect("ProjectSettings marker");
    root
}

#[test]
fn help_and_version_use_standard_success_output() {
    let help = run_cli(&["--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(
        String::from_utf8(help.stdout)
            .expect("help is UTF-8")
            .contains("Usage:")
    );

    let version = run_cli(&["--version"]);
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert!(
        String::from_utf8(version.stdout)
            .expect("version is UTF-8")
            .contains(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_project_root_is_a_structured_input_error() {
    let root = tempfile::tempdir().expect("temporary non-project");
    let output = run_cli(&[
        "--project-root",
        root.path().to_str().expect("UTF-8 test path"),
        "capabilities",
    ]);

    let error = assert_failure(&output, "input", 3);
    assert_eq!(error["error"]["code"], "invalid_request");
}

#[test]
fn missing_daemon_is_a_retryable_structured_unavailable_error() {
    let root = unity_project();
    let output = run_cli(&[
        "--project-root",
        root.path().to_str().expect("UTF-8 test path"),
        "capabilities",
    ]);

    let error = assert_failure(&output, "unavailable", 4);
    assert_eq!(error["error"]["code"], "not_ready");
    assert_eq!(error["error"]["retryable"], true);
}

#[test]
fn malformed_and_unknown_request_json_are_structured_input_errors() {
    let directory = tempfile::tempdir().expect("temporary input directory");
    for (name, body) in [
        ("malformed.json", "{"),
        (
            "unknown.json",
            r#"{"cli_contract_version":1,"operation":{"kind":"status","request":{}},"unknown":true}"#,
        ),
    ] {
        let path = directory.path().join(name);
        fs::write(&path, body).expect("write request fixture");
        let output = run_cli(&["--request-json", path.to_str().expect("UTF-8 test path")]);
        assert_failure(&output, "input", 3);
    }
}

#[test]
fn oversized_stdin_is_rejected_before_protocol_or_project_work() {
    let input = vec![b' '; 512 * 1024 + 1];
    let output = run_cli_with_stdin(&["--request-json", "-"], &input);
    let error = assert_failure(&output, "input", 3);
    assert!(
        error["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("encoded input exceeded its limit")
    );
}

#[test]
fn request_json_and_subcommand_conflict_is_machine_readable() {
    let directory = tempfile::tempdir().expect("temporary input directory");
    let path = directory.path().join("request.json");
    fs::write(
        &path,
        r#"{"cli_contract_version":1,"operation":{"kind":"status","request":{}}}"#,
    )
    .expect("write request fixture");
    let output = run_cli(&[
        "--request-json",
        path.to_str().expect("UTF-8 test path"),
        "status",
    ]);

    assert_failure(&output, "usage", 2);
}
