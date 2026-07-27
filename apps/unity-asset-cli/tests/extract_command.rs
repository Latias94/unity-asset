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
fn extract_dry_run_is_side_effect_free_and_execution_uses_manifest_paths() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let input = sample("banner_1");
    let request = write_request(temp.path());

    let dry_run = extract(&input, &output_root, &request, true);
    assert_success(&dry_run);
    let plan: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(plan["contract"], "unity_asset.extraction_plan");
    assert_eq!(plan["version"], 1);
    assert_eq!(plan["artifacts"].as_array().unwrap().len(), 1);
    assert!(!output_root.exists());

    let execution = extract(&input, &output_root, &request, false);
    assert_success(&execution);
    let report = extraction_report(&execution);
    assert_eq!(report["contract"], "unity_asset.extraction_report");
    assert_eq!(report["version"], 1);
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
    assert!(help.contains("\n  extract "));
    assert!(help.contains("\n  split-yaml "));
    assert!(!help.contains("\n  export "));
    assert!(!help.contains("\n  export-bundle "));
    assert!(!help.contains("\n  export-serialized "));
}

#[test]
fn finite_policy_values_are_rejected_as_argument_errors() {
    let output = run([
        "extract",
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
fn extract_resume_reuses_verified_outputs_across_independent_processes() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let manifest_path = temp.path().join("manifest.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());

    let first = extract(&input, &output_root, &request, false);
    assert_success(&first);
    write_resume_manifest(&manifest_path, &first);

    let resumed = extract_with_resume(&input, &output_root, &request, &manifest_path);
    assert_success(&resumed);
    let report = extraction_report(&resumed);
    assert_eq!(report["counts"]["resumed"], 1);
    let artifacts = report["manifest"]["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["status"], "resumed");
}

#[test]
fn extract_atomically_publishes_a_durable_resume_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let manifest_relative = Path::new("reports/extraction-manifest.json");
    let manifest_path = output_root.join(manifest_relative);
    let input = sample("banner_1");
    let request = write_request(temp.path());
    let read_only_stdout = temp.path().join("read-only-stdout");
    fs::write(&read_only_stdout, b"unchanged").unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_unity-asset"))
        .arg("extract")
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
        .expect("the extract command must start");
    assert!(
        !status.success(),
        "the injected stdout failure must surface"
    );
    let first: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(first["artifacts"][0]["status"], "written");
    assert_eq!(fs::read(&read_only_stdout).unwrap(), b"unchanged");

    let resumed = extract_with_manifest(
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
fn extraction_plan_is_replayable_without_replanning_and_can_resume() {
    let temp = tempfile::tempdir().unwrap();
    let output_root = temp.path().join("artifacts");
    let plan_path = temp.path().join("plan.json");
    let manifest_path = temp.path().join("manifest.json");
    let input = sample("banner_1");
    let request = write_request(temp.path());

    let dry_run = extract(&input, &output_root, &request, true);
    assert_success(&dry_run);
    fs::write(&plan_path, &dry_run.stdout).unwrap();

    let executed = extract_with_plan(&input, &output_root, &plan_path, None);
    assert_success(&executed);
    let report = extraction_report(&executed);
    assert_eq!(report["manifest"]["artifacts"][0]["status"], "written");
    write_resume_manifest(&manifest_path, &executed);

    let resumed = extract_with_plan(&input, &output_root, &plan_path, Some(&manifest_path));
    assert_success(&resumed);
    let report = extraction_report(&resumed);
    assert_eq!(report["manifest"]["artifacts"][0]["status"], "resumed");
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
    let path = directory.join("request.json");
    let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly)
        .with_filter(ExtractionFilter::new([28], None, None, Some(1)).unwrap());
    let file = fs::File::create(&path).unwrap();
    request.write_canonical_json(file).unwrap();
    path
}

fn extract(input: &Path, output: &Path, request: &Path, dry_run: bool) -> Output {
    extract_with_inputs(input, output, dry_run, Some(request), None, None, None)
}

fn extract_with_resume(input: &Path, output: &Path, request: &Path, resume: &Path) -> Output {
    extract_with_inputs(
        input,
        output,
        false,
        Some(request),
        Some(resume),
        None,
        None,
    )
}

fn extract_with_plan(input: &Path, output: &Path, plan: &Path, resume: Option<&Path>) -> Output {
    extract_with_inputs(input, output, false, None, resume, Some(plan), None)
}

fn extract_with_manifest(
    input: &Path,
    output: &Path,
    request: &Path,
    manifest: &Path,
    resume: Option<&Path>,
) -> Output {
    extract_with_inputs(
        input,
        output,
        false,
        Some(request),
        resume,
        None,
        Some(manifest),
    )
}

fn extract_with_inputs(
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
        .arg("extract")
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
    command.output().expect("the extract command must start")
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
    assert_eq!(report["version"], 1);
    assert_eq!(
        report["manifest"]["contract"],
        "unity_asset.extraction_manifest"
    );
    report
}

fn write_resume_manifest(path: &Path, output: &Output) {
    let report = extraction_report(output);
    fs::write(path, serde_json::to_vec(&report["manifest"]).unwrap())
        .expect("write extraction resume manifest");
}
