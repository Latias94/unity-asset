use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use unity_asset::extraction::{
    ExtractionFilter, ExtractionRepresentationPolicy, ExtractionRequest,
};

const SPLIT_SOURCE: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1001
GameObject:
  m_Name: Alpha
--- !u!4 &1002
Transform:
  m_GameObject: {fileID: 1001}
"#;

fn sample(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/samples")
        .join(name)
}

fn run(arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unity-asset"))
        .args(arguments)
        .output()
        .expect("the built unity-asset binary must start")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn export_dry_run_is_side_effect_free_and_execution_uses_manifest_paths() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let input = sample("banner_1");
    let request = write_request(temp.path());

    let dry_run = export_artifacts(&input, &output_root, &request, true);
    assert_success(&dry_run);
    let plan: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(plan["contract"], "unity_asset.extraction_plan");
    assert_eq!(plan["version"], 4);
    assert_eq!(plan["artifacts"].as_array().unwrap().len(), 1);
    assert!(!output_root.exists());

    let execution = export_artifacts(&input, &output_root, &request, false);
    assert_success(&execution);
    let report = extraction_report(&execution);
    assert_eq!(report["contract"], "unity_asset.extraction_report");
    assert_eq!(report["version"], 3);
    assert_eq!(report["counts"]["written"], 1);
    let artifacts = report["manifest"]["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["status"], "written");
    let relative_path = artifacts[0]["path"].as_str().unwrap();
    assert!(output_root.join(relative_path).is_file());
}

#[test]
fn help_exposes_typed_workspace_reference_and_extraction_commands() {
    let output = run(["--help"]);
    assert_success(&output);
    let help = String::from_utf8(output.stdout).unwrap();

    assert!(help.contains("\n  workspace "));
    assert!(help.contains("\n  references "));
    assert!(help.contains("\n  export "));
    assert!(help.contains("\n  split-yaml "));
    assert!(!help.contains("\n  extract "));
    assert!(!help.contains("\n  export-bundle "));
    assert!(!help.contains("\n  export-serialized "));

    let export_help = run(["export", "--help"]);
    assert_success(&export_help);
    assert!(
        String::from_utf8(export_help.stdout)
            .unwrap()
            .contains("safe publication minimum is 5")
    );
}

#[test]
fn finite_policy_values_are_rejected_as_argument_errors() {
    let output = run([
        "export",
        "--input",
        "input",
        "--output",
        "output",
        "--request",
        "request.json",
        "--failure",
        "keep-going-somehow",
    ]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("argument error must be JSON");
    assert_eq!(error["contract"], "unity_asset.cli_error");
    assert_eq!(error["code"], "CLI_ARGUMENT_ERROR");
}

#[test]
fn export_rejects_structured_inputs_that_share_stdin_with_stable_details() {
    let output = run([
        "export",
        "--input",
        "input",
        "--output",
        "output",
        "--request",
        "-",
        "--resume",
        "-",
    ]);

    let error = assert_cli_error(&output, "CLI_EXPORT_ARGUMENT_INVALID");
    assert_eq!(error["details"]["kind"], "structured_inputs_share_stdin");
    assert_eq!(
        error["details"]["inputs"],
        serde_json::json!(["--request", "--resume"])
    );
}

#[test]
fn export_reports_missing_input_with_stable_machine_details() {
    let temp = tempfile::tempdir().unwrap();
    let request = write_request(temp.path());
    let missing = temp.path().join("missing.assets");
    let output = temp.path().join("artifacts");

    let result = export_artifacts(&missing, &output, &request, true);

    let error = assert_cli_error(&result, "CLI_EXPORT_SOURCE_CHANGED");
    assert_eq!(error["details"]["kind"], "workspace_load_failed");
    assert_eq!(error["details"]["input"]["encoding"], "utf8");
    assert!(!output.exists());
}

#[test]
fn export_rejects_invalid_manifest_paths_as_typed_output_errors() {
    let output = run([
        "export",
        "--input",
        "input",
        "--output",
        "output",
        "--request",
        "request.json",
        "--manifest",
        "../manifest.json",
    ]);

    let error = assert_cli_error(&output, "CLI_EXPORT_OUTPUT_INVALID");
    assert_eq!(error["details"]["kind"], "manifest_path_invalid");
    assert_eq!(error["details"]["path"]["encoding"], "utf8");
    assert_eq!(error["details"]["path"]["value"], "../manifest.json");
}

#[cfg(any(unix, windows))]
#[test]
fn export_rejects_non_utf8_manifest_paths_as_typed_output_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_unity-asset"))
        .arg("export")
        .arg("--input")
        .arg("input")
        .arg("--output")
        .arg("output")
        .arg("--request")
        .arg("request.json")
        .arg("--manifest")
        .arg(non_utf8_path())
        .output()
        .expect("the export command must start");

    let error = assert_cli_error(&output, "CLI_EXPORT_OUTPUT_INVALID");
    assert_eq!(error["details"]["kind"], "manifest_path_non_utf8");
    assert_ne!(error["details"]["path"]["encoding"], "utf8");
    assert!(error["details"]["path"]["value"].is_array());
}

#[test]
fn export_resume_reuses_verified_outputs_across_independent_processes() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let manifest_path = temp.path().join("manifest.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());

    let first = export_artifacts(&input, &output_root, &request, false);
    assert_success(&first);
    write_resume_manifest(&manifest_path, &first);

    let resumed = export_with_resume(&input, &output_root, &request, &manifest_path);
    assert_success(&resumed);
    let report = extraction_report(&resumed);
    assert_eq!(report["counts"]["resumed"], 1);
    let artifacts = report["manifest"]["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["status"], "resumed");
}

#[test]
fn export_can_raise_the_evidence_verification_limit_to_recover() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let input = sample("banner_1");
    let request = write_request(temp.path());

    let first = export_artifacts(&input, &output_root, &request, false);
    assert_success(&first);
    let report = extraction_report(&first);
    let artifact_path = report["manifest"]["artifacts"][0]["path"].as_str().unwrap();
    let artifact_length = fs::metadata(output_root.join(artifact_path)).unwrap().len();
    assert!(artifact_length > 1);

    let rejected = export_with_evidence_verification_limit(
        &input,
        &output_root,
        &request,
        artifact_length - 1,
    );
    let error = assert_cli_error(&rejected, "CLI_EXPORT_RESOURCE_LIMIT");
    assert_eq!(
        error["details"]["kind"],
        "evidence_verification_limit_exceeded"
    );
    assert_eq!(error["details"]["required"], artifact_length);
    assert_eq!(error["details"]["remaining"], artifact_length - 1);

    let recovered = export_with_evidence_verification_limit(
        &input,
        &output_root,
        &request,
        artifact_length.checked_mul(2).unwrap(),
    );
    assert_success(&recovered);
    let report = extraction_report(&recovered);
    assert_eq!(
        report["manifest"]["artifacts"][0]["status"],
        "skipped_existing"
    );
}

#[test]
fn export_atomically_publishes_a_durable_resume_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let manifest_relative = Path::new("reports/extraction-manifest.json");
    let manifest_path = output_root.join(manifest_relative);
    let input = sample("banner_1");
    let request = write_request(temp.path());
    let read_only_stdout = temp.path().join("read-only-stdout");
    fs::write(&read_only_stdout, b"unchanged").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_unity-asset"))
        .arg("export")
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&output_root)
        .arg("--request")
        .arg(&request)
        .arg("--manifest")
        .arg(manifest_relative)
        .stdout(Stdio::from(fs::File::open(&read_only_stdout).unwrap()))
        .status()
        .expect("the export command must start");
    assert!(
        !status.success(),
        "the injected stdout failure must surface"
    );
    let first: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(first["artifacts"][0]["status"], "written");
    assert_eq!(fs::read(&read_only_stdout).unwrap(), b"unchanged");

    let resumed = export_with_manifest(
        &input,
        &output_root,
        &request,
        manifest_relative,
        Some(&manifest_path),
    );
    assert_success(&resumed);
    let report = extraction_report(&resumed);
    assert_eq!(report["manifest"]["artifacts"][0]["status"], "resumed");
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(persisted, report["manifest"]);
    assert_no_staging_files(&output_root);
}

#[test]
fn export_plan_is_replayable_without_replanning_and_can_resume() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let plan_path = temp.path().join("plan.json");
    let manifest_path = temp.path().join("manifest.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());

    let dry_run = export_artifacts(&input, &output_root, &request, true);
    assert_success(&dry_run);
    fs::write(&plan_path, &dry_run.stdout).unwrap();

    let executed = export_with_plan(&input, &output_root, &plan_path, None);
    assert_success(&executed);
    let report = extraction_report(&executed);
    assert_eq!(report["manifest"]["artifacts"][0]["status"], "written");
    write_resume_manifest(&manifest_path, &executed);

    let resumed = export_with_plan(&input, &output_root, &plan_path, Some(&manifest_path));
    assert_success(&resumed);
    let report = extraction_report(&resumed);
    assert_eq!(report["manifest"]["artifacts"][0]["status"], "resumed");
}

#[test]
fn export_rejects_version_one_plans_before_creating_output() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let plan_path = temp.path().join("plan-v1.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());
    let mut plan = dry_run_plan(&input, &output_root, &request);
    plan["version"] = serde_json::Value::from(1);
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    let rejected = export_with_plan(&input, &output_root, &plan_path, None);

    assert_cli_error(&rejected, "CLI_CONTRACT_INVALID");
    assert!(!output_root.exists());
}

#[test]
fn export_rejects_version_one_resume_manifests_before_creating_output() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let replay_output = temp.path().join("replay-artifacts");
    let manifest_path = temp.path().join("manifest-v1.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());
    let executed = export_artifacts(&input, &output_root, &request, false);
    assert_success(&executed);
    let mut manifest = extraction_report(&executed)["manifest"].clone();
    manifest["version"] = serde_json::Value::from(1);
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let rejected = export_with_resume(&input, &replay_output, &request, &manifest_path);

    assert_cli_error(&rejected, "CLI_CONTRACT_INVALID");
    assert!(!replay_output.exists());
}

#[test]
fn extraction_report_reader_rejects_version_one() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let input = sample("banner_1");
    let request = write_request(temp.path());
    let executed = export_artifacts(&input, &output_root, &request, false);
    assert_success(&executed);
    let mut report = extraction_report(&executed);
    report["version"] = serde_json::Value::from(1);
    let encoded = serde_json::to_vec(&report).unwrap();

    let error = unity_asset::extraction::ExtractionReport::read_json(
        encoded.as_slice(),
        &mut unity_asset::AssetLoadBudget::default(),
    )
    .expect_err("version one reports must not enter the current contract");

    assert!(
        error
            .to_string()
            .contains("report version 1 is unsupported")
    );
}

#[test]
fn export_rejects_tampered_working_set_as_a_plan_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let plan_path = temp.path().join("plan.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());

    let dry_run = export_artifacts(&input, &output_root, &request, true);
    assert_success(&dry_run);
    let mut plan: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    plan["artifacts"][0]["working_set_bytes"] = serde_json::Value::from(1);
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    let rejected = export_with_plan(&input, &output_root, &plan_path, None);

    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&rejected.stderr).unwrap();
    assert_eq!(error["contract"], "unity_asset.cli_error");
    assert_eq!(error["code"], "CLI_EXPORT_PLAN_REJECTED");
    assert_eq!(error["details"]["kind"], "plan_derivation_mismatch");
    assert_eq!(error["details"]["mismatch"], "representations");
    assert!(!output_root.exists());
}

#[test]
fn export_reports_required_representation_unavailability_with_stable_details() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let input = sample("char_118_yuki.ab");
    let request = write_request_for_class(
        temp.path(),
        142,
        ExtractionRepresentationPolicy::RequireDecoded,
    );

    let rejected = export_artifacts(&input, &output_root, &request, true);

    let error = assert_cli_error(&rejected, "CLI_EXPORT_REPRESENTATION_UNAVAILABLE");
    assert_eq!(error["details"]["kind"], "representation_unavailable");
    assert!(error["details"]["address"].is_object());
    assert!(error["details"]["diagnostic"].is_string());
    assert!(!output_root.exists());
}

#[test]
fn export_reports_workspace_revision_drift_with_stable_details() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let plan_path = temp.path().join("plan.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());
    let mut plan = dry_run_plan(&input, &output_root, &request);
    let changed_revision = different_digest(&plan["revision"]);
    plan["revision"] = changed_revision;
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    let rejected = export_with_plan(&input, &output_root, &plan_path, None);

    let error = assert_cli_error(&rejected, "CLI_EXPORT_WORKSPACE_MISMATCH");
    assert_eq!(error["details"]["kind"], "workspace_revision_mismatch");
    assert!(!output_root.exists());
}

#[test]
fn export_reports_source_drift_with_stable_details() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let plan_path = temp.path().join("plan.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());
    let mut plan = dry_run_plan(&input, &output_root, &request);
    let changed_fingerprint = different_digest(&plan["sources"][0]["fingerprint"]["digest"]);
    plan["sources"][0]["fingerprint"]["digest"] = changed_fingerprint;
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

    let rejected = export_with_plan(&input, &output_root, &plan_path, None);

    let error = assert_cli_error(&rejected, "CLI_EXPORT_SOURCE_CHANGED");
    assert_eq!(error["details"]["kind"], "source_changed");
    assert!(error["details"]["locator"].is_object());
    assert!(!output_root.exists());
}

#[test]
fn export_reports_resume_plan_mismatch_with_stable_details() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let replay_output = temp.path().join("replay-artifacts");
    let plan_path = temp.path().join("plan.json");
    let manifest_path = temp.path().join("manifest.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());
    let plan = dry_run_plan(&input, &output_root, &request);
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let executed = export_with_plan(&input, &output_root, &plan_path, None);
    assert_success(&executed);
    let mut manifest = extraction_report(&executed)["manifest"].clone();
    let changed_plan_digest = different_digest(&manifest["plan_digest"]);
    manifest["plan_digest"] = changed_plan_digest;
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let rejected = export_with_plan(&input, &replay_output, &plan_path, Some(&manifest_path));

    let error = assert_cli_error(&rejected, "CLI_EXPORT_RESUME_MISMATCH");
    assert_eq!(error["details"]["kind"], "resume_plan_mismatch");
    assert!(!replay_output.exists());
}

#[test]
fn removed_extract_alias_is_a_structured_argument_error() {
    let output = run(["extract", "--help"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["code"], "CLI_ARGUMENT_ERROR");
}

#[test]
fn split_yaml_cli_publishes_a_separate_json_report_and_replaces_documents() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("scene.prefab");
    let output = temp.path().join("documents");
    fs::write(&input, SPLIT_SOURCE).unwrap();

    let first = split_yaml(&input, &output, None);
    assert_success(&first);
    let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["contract"], "unity_asset.yaml_split_report");
    assert_eq!(report["version"], 1);
    assert_eq!(report["written"], 2);
    assert_eq!(report["skipped_existing"], 0);

    let document = output.join("documents/scene.prefab/anchor-1001.yaml");
    assert!(document.is_file());
    assert!(
        output
            .join("documents/scene.prefab/anchor-1002.yaml")
            .is_file()
    );
    assert!(!output.join("extraction-manifest.json").exists());

    fs::write(&document, b"stale output").unwrap();
    let replaced = split_yaml(&input, &output, Some("replace"));
    assert_success(&replaced);
    let report: serde_json::Value = serde_json::from_slice(&replaced.stdout).unwrap();
    assert_eq!(report["written"], 2);
    assert_eq!(report["skipped_existing"], 0);
    assert!(
        String::from_utf8(fs::read(&document).unwrap())
            .unwrap()
            .contains("Alpha")
    );
    assert_no_staging_files(&output);
}

fn write_request(directory: &Path) -> PathBuf {
    write_request_for_class(directory, 28, ExtractionRepresentationPolicy::RawOnly)
}

fn write_request_for_class(
    directory: &Path,
    class_id: i32,
    representation: ExtractionRepresentationPolicy,
) -> PathBuf {
    let path = directory.join("request.json");
    let request = ExtractionRequest::all(representation)
        .with_filter(ExtractionFilter::new([class_id], None, None, Some(1)).unwrap());
    let file = fs::File::create(&path).unwrap();
    request.write_canonical_json(file).unwrap();
    path
}

fn dry_run_plan(input: &Path, output: &Path, request: &Path) -> serde_json::Value {
    let dry_run = export_artifacts(input, output, request, true);
    assert_success(&dry_run);
    serde_json::from_slice(&dry_run.stdout).unwrap()
}

fn different_digest(value: &serde_json::Value) -> serde_json::Value {
    const ZERO: &str = "blake3-v1:0000000000000000000000000000000000000000000000000000000000000000";
    const ONE: &str = "blake3-v1:1111111111111111111111111111111111111111111111111111111111111111";
    serde_json::Value::from(if value.as_str() == Some(ZERO) {
        ONE
    } else {
        ZERO
    })
}

fn export_artifacts(input: &Path, output: &Path, request: &Path, dry_run: bool) -> Output {
    export_with_inputs(input, output, dry_run, Some(request), None, None, None)
}

fn export_with_resume(input: &Path, output: &Path, request: &Path, resume: &Path) -> Output {
    export_with_inputs(
        input,
        output,
        false,
        Some(request),
        Some(resume),
        None,
        None,
    )
}

fn export_with_plan(input: &Path, output: &Path, plan: &Path, resume: Option<&Path>) -> Output {
    export_with_inputs(input, output, false, None, resume, Some(plan), None)
}

fn export_with_evidence_verification_limit(
    input: &Path,
    output: &Path,
    request: &Path,
    limit: u64,
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unity-asset"))
        .arg("export")
        .arg("--input")
        .arg(input)
        .arg("--output")
        .arg(output)
        .arg("--request")
        .arg(request)
        .arg("--existing-output")
        .arg("skip")
        .arg("--max-evidence-verification-bytes")
        .arg(limit.to_string())
        .output()
        .expect("the export command must start")
}

fn export_with_manifest(
    input: &Path,
    output: &Path,
    request: &Path,
    manifest: &Path,
    resume: Option<&Path>,
) -> Output {
    export_with_inputs(
        input,
        output,
        false,
        Some(request),
        resume,
        None,
        Some(manifest),
    )
}

fn export_with_inputs(
    input: &Path,
    output: &Path,
    dry_run: bool,
    request: Option<&Path>,
    resume: Option<&Path>,
    plan: Option<&Path>,
    manifest: Option<&Path>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_unity-asset"));
    command
        .arg("export")
        .arg("--input")
        .arg(input)
        .arg("--output")
        .arg(output);
    if let Some(request) = request {
        command.arg("--request").arg(request);
    }
    if dry_run {
        command.arg("--dry-run");
    }
    if let Some(resume) = resume {
        command.arg("--resume").arg(resume);
    }
    if let Some(plan) = plan {
        command.arg("--plan").arg(plan);
    }
    if let Some(manifest) = manifest {
        command.arg("--manifest").arg(manifest);
    }
    command.output().expect("the export command must start")
}

fn split_yaml(input: &Path, output: &Path, existing_output: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_unity-asset"));
    command
        .arg("split-yaml")
        .arg("--input")
        .arg(input)
        .arg("--output")
        .arg(output);
    if let Some(existing_output) = existing_output {
        command.arg("--existing-output").arg(existing_output);
    }
    command.output().expect("the split-yaml command must start")
}

fn assert_no_staging_files(root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            assert_no_staging_files(&path);
        } else {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !(name.starts_with(".unity-asset-") && name.ends_with(".tmp")),
                "staging output leaked at {}",
                path.display()
            );
        }
    }
}

fn extraction_report(output: &Output) -> serde_json::Value {
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("extraction report must be JSON");
    assert_eq!(report["contract"], "unity_asset.extraction_report");
    assert_eq!(report["version"], 3);
    assert_eq!(
        report["manifest"]["contract"],
        "unity_asset.extraction_manifest"
    );
    assert_eq!(report["manifest"]["version"], 3);
    report
}

fn assert_cli_error(output: &Output, code: &str) -> serde_json::Value {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("CLI error must be JSON");
    assert_eq!(error["contract"], "unity_asset.cli_error");
    assert_eq!(error["version"], 2);
    assert_eq!(error["code"], code);
    error
}

#[cfg(unix)]
fn non_utf8_path() -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    PathBuf::from(OsString::from_vec(vec![b'm', 0x80]))
}

#[cfg(windows)]
fn non_utf8_path() -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;

    PathBuf::from(OsString::from_wide(&[u16::from(b'm'), 0xd800]))
}

fn write_resume_manifest(path: &Path, output: &Output) {
    let report = extraction_report(output);
    fs::write(path, serde_json::to_vec(&report["manifest"]).unwrap())
        .expect("write extraction resume manifest");
}
