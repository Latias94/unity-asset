use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use unity_asset::schema::SchemaRecipePlanner;
use unity_asset::workspace::{
    AssetWorkspace, COMMIT_REPORT_VERSION, MutationPlanBuilder, MutationValue,
    PREPARE_REPORT_VERSION, RECOVERY_DISCOVERY_VERSION, RECOVERY_OUTCOME_VERSION,
    SourceOpenRequest, WorkspaceOptions, workspace_capabilities,
};
use unity_asset::{
    AssetLoadBudget, FieldPath, ObjectAddress, SourceAlias, SourceKind, SourceLocator, WorkspaceId,
};

const WORKSPACE_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1001
GameObject:
  m_Name: Alpha
  m_IsActive: 1
--- !u!4 &1002
Transform:
  m_GameObject: {fileID: 1001}
  m_LocalPosition: {x: 1, y: 2, z: 3}
"#;

const SERIALIZED_FILE_FIXTURE: &[u8] = include_bytes!(
    "../../../crates/unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin"
);

fn run(arguments: &[&OsStr]) -> Output {
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
    assert!(
        output.stderr.is_empty(),
        "successful command wrote stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_cli_error(output: &Output, code: &str) -> Value {
    assert!(
        !output.status.success(),
        "failing command unexpectedly succeeded"
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
    assert!(report["warnings"].is_array());
    report
}

fn write_workspace(root: &Path) -> PathBuf {
    let path = root.join("scene.prefab");
    fs::write(&path, WORKSPACE_YAML).expect("workspace fixture must be writable");
    path
}

fn write_mutation_contracts(root: &Path, input: &Path) -> (PathBuf, PathBuf) {
    let workspace_id =
        WorkspaceId::from_u128(0x434c_495f_574f_524b_464c_4f57).expect("fixed workspace ID");
    let mut workspace =
        AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default())
            .expect("workspace");
    workspace
        .load_source(
            SourceOpenRequest::new(
                input,
                SourceAlias::new("scene.prefab").expect("source alias"),
            )
            .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .expect("load source");
    let snapshot = workspace.snapshot();
    let address =
        ObjectAddress::yaml(SourceLocator::path("scene.prefab").unwrap(), "1001").unwrap();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let observed = planner
        .inspect(&address, &mut AssetLoadBudget::default())
        .expect("inspect object");
    let fragment = planner
        .lower_field_replace(
            &observed,
            FieldPath::root().push_field("m_Name").unwrap(),
            MutationValue::string("Committed").unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .expect("lower mutation");
    let mut builder = MutationPlanBuilder::new(snapshot.workspace_id(), snapshot.revision());
    builder.append(fragment).expect("append plan fragment");
    let plan = builder.build().expect("build mutation plan");

    let plan_path = root.join("mutation-plan.json");
    fs::write(&plan_path, plan.canonical_json().unwrap()).expect("write mutation plan");
    let address_path = root.join("object-address.json");
    fs::write(&address_path, serde_json::to_vec(&address).unwrap()).expect("write object address");
    (plan_path, address_path)
}

fn truncate_recovery_events_after(recovery: &Value, retained_type: &str) {
    let root: PathBuf = serde_json::from_value(recovery["root"].clone())
        .expect("recovery root must be a platform path");
    let events = root.join("events");
    let mut paths = fs::read_dir(&events)
        .expect("recovery event directory must exist")
        .map(|entry| entry.expect("recovery event entry").path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    paths.sort_unstable();
    let cutoff = paths
        .iter()
        .rposition(|path| {
            let event: Value = serde_json::from_slice(
                &fs::read(path).expect("recovery event must remain readable"),
            )
            .expect("recovery event must be JSON");
            event["kind"]["type"] == retained_type
        })
        .expect("requested recovery barrier must exist");
    assert!(
        cutoff + 1 < paths.len(),
        "crash simulation must remove a non-empty event suffix"
    );
    for path in &paths[cutoff + 1..] {
        fs::remove_file(path).expect("truncate recovery event suffix");
    }
}

#[test]
fn workspace_capabilities_emit_the_stable_library_contract() {
    let arguments = [OsStr::new("workspace"), OsStr::new("capabilities")];
    let first = run(&arguments);
    let second = run(&arguments);
    assert_success(&first);
    assert_success(&second);
    assert_eq!(first.stdout, second.stdout);

    let mut expected =
        serde_json::to_vec(&workspace_capabilities()).expect("capability catalog must serialize");
    expected.push(b'\n');
    assert_eq!(first.stdout, expected);

    let catalog: Value =
        serde_json::from_slice(&first.stdout).expect("capability output must be JSON");
    assert_eq!(catalog["contract"], "unity_asset.workspace_capabilities");
    assert_eq!(catalog["contract_version"], 1);
    assert_eq!(catalog["contracts"]["mutation_plan"], 2);
    assert_eq!(catalog["contracts"]["reference_graph_projection"], 1);
    assert_eq!(catalog["automation"]["structured_input"], true);
    assert_eq!(catalog["automation"]["display_text_input"], false);
    assert_eq!(catalog["automation"]["generic_command_bus"], false);
}

#[test]
fn workspace_inspection_emits_versioned_sources_and_sorted_objects() {
    let temp = tempfile::tempdir().expect("temporary workspace must be available");
    let input = write_workspace(temp.path());

    let sources = run(&[
        OsStr::new("workspace"),
        OsStr::new("inspect"),
        OsStr::new("sources"),
        OsStr::new("--input"),
        input.as_os_str(),
    ]);
    assert_success(&sources);
    let sources: Value =
        serde_json::from_slice(&sources.stdout).expect("source inspection must be JSON");
    let sources = sources
        .as_array()
        .expect("source inspection must be an array");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["version"], 1);
    assert_eq!(sources[0]["kind"], "yaml");
    assert_eq!(
        sources[0]["encoded_length"],
        u64::try_from(WORKSPACE_YAML.len()).unwrap()
    );
    assert_eq!(sources[0]["format"]["kind"], "yaml");
    assert_eq!(sources[0]["format"]["summary"]["document_count"], 2);

    let objects = run(&[
        OsStr::new("workspace"),
        OsStr::new("inspect"),
        OsStr::new("objects"),
        OsStr::new("--input"),
        input.as_os_str(),
    ]);
    assert_success(&objects);
    let objects: Value =
        serde_json::from_slice(&objects.stdout).expect("object inspection must be JSON");
    let objects = objects
        .as_array()
        .expect("object inspection must be an array");
    assert_eq!(objects.len(), 2);

    for object in objects {
        assert_eq!(object["version"], 1);
        assert_eq!(object["format"]["kind"], "yaml");
        assert_eq!(object["workspace_id"], objects[0]["workspace_id"]);
        assert_eq!(object["revision"], objects[0]["revision"]);
    }
    assert_eq!(objects[0]["class"]["class_id"], 1);
    assert_eq!(objects[0]["class"]["class_name"], "GameObject");
    assert_eq!(objects[0]["class"]["anchor"], "1001");
    assert_eq!(objects[0]["format"]["document_index"], 0);
    assert_eq!(objects[1]["class"]["class_id"], 4);
    assert_eq!(objects[1]["class"]["class_name"], "Transform");
    assert_eq!(objects[1]["class"]["anchor"], "1002");
    assert_eq!(objects[1]["format"]["document_index"], 1);
}

#[test]
fn workspace_inspection_loads_streamed_resource_companions_for_directory_and_file_inputs() {
    let temp = tempfile::tempdir().expect("temporary workspace must be available");
    let serialized = temp.path().join("main.assets");
    fs::write(&serialized, SERIALIZED_FILE_FIXTURE).expect("serialized fixture must be writable");
    fs::write(temp.path().join("CAB-main.resS"), b"streamed-resS")
        .expect("resS companion must be writable");
    fs::write(
        temp.path().join("CAB-secondary.resource"),
        b"streamed-resource",
    )
    .expect("resource companion must be writable");

    for input in [temp.path(), serialized.as_path()] {
        let sources = run(&[
            OsStr::new("workspace"),
            OsStr::new("inspect"),
            OsStr::new("sources"),
            OsStr::new("--input"),
            input.as_os_str(),
        ]);
        assert_success(&sources);
        let sources: Value =
            serde_json::from_slice(&sources.stdout).expect("source inspection must be JSON");
        let sources = sources
            .as_array()
            .expect("source inspection must be an array");
        assert_eq!(sources.len(), 3);
        assert_eq!(
            sources
                .iter()
                .filter(|source| source["kind"].as_str() == Some("streamed_resource"))
                .count(),
            2
        );
        assert!(
            sources
                .iter()
                .any(|source| source["kind"].as_str() == Some("serialized_file"))
        );
    }
}

#[test]
fn missing_workspace_object_has_a_stable_structured_error() {
    let temp = tempfile::tempdir().expect("temporary workspace must be available");
    let input = write_workspace(temp.path());
    let address =
        ObjectAddress::yaml(SourceLocator::path("scene.prefab").unwrap(), "9999").unwrap();
    let address_path = temp.path().join("missing-object-address.json");
    fs::write(&address_path, serde_json::to_vec(&address).unwrap()).unwrap();

    let output = run(&[
        OsStr::new("workspace"),
        OsStr::new("inspect"),
        OsStr::new("object"),
        OsStr::new("--input"),
        input.as_os_str(),
        OsStr::new("--address-json"),
        address_path.as_os_str(),
    ]);
    let report = assert_cli_error(&output, "CLI_WORKSPACE_LOOKUP_MISSING");
    assert_eq!(report["details"]["kind"], "missing");
}

#[test]
fn load_warning_is_embedded_in_the_single_structured_failure() {
    let temp = tempfile::tempdir().expect("temporary workspace must be available");
    let address =
        ObjectAddress::yaml(SourceLocator::path("scene.prefab").unwrap(), "1001").unwrap();
    let address_path = temp.path().join("missing-source-address.json");
    fs::write(&address_path, serde_json::to_vec(&address).unwrap()).unwrap();

    let output = run(&[
        OsStr::new("--show-warnings"),
        OsStr::new("workspace"),
        OsStr::new("inspect"),
        OsStr::new("object"),
        OsStr::new("--input"),
        temp.path().as_os_str(),
        OsStr::new("--address-json"),
        address_path.as_os_str(),
    ]);
    let report = assert_cli_error(&output, "CLI_WORKSPACE_LOOKUP_UNLOADED");
    let warnings = report["warnings"]
        .as_array()
        .expect("warnings must remain structured");
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0]
            .as_str()
            .expect("warning must be text")
            .contains("no supported Unity sources found")
    );
}

#[test]
fn prepare_rejection_preserves_the_typed_failure_report() {
    let temp = tempfile::tempdir().expect("temporary workspace must be available");
    let input = write_workspace(temp.path());
    let (plan, _) = write_mutation_contracts(temp.path(), &input);
    fs::write(&input, WORKSPACE_YAML.replace("Alpha", "Changed"))
        .expect("mutated workspace fixture must be writable");

    let output = run(&[
        OsStr::new("workspace"),
        OsStr::new("prepare"),
        OsStr::new("--input"),
        input.as_os_str(),
        OsStr::new("--plan"),
        plan.as_os_str(),
    ]);
    let report = assert_cli_error(&output, "CLI_WORKSPACE_PREPARE_REJECTED");
    assert_eq!(
        report["details"]["report"]["version"],
        PREPARE_REPORT_VERSION
    );
    assert_eq!(
        report["details"]["report"]["diagnostics"][0]["diagnostic"]["code"],
        "PREPARE_REVISION_MISMATCH"
    );
}

#[test]
fn references_graph_emits_the_versioned_resolved_projection() {
    let temp = tempfile::tempdir().expect("temporary workspace must be available");
    let input = write_workspace(temp.path());
    let output = run(&[
        OsStr::new("references"),
        OsStr::new("graph"),
        OsStr::new("--input"),
        input.as_os_str(),
    ]);
    assert_success(&output);

    let graph: Value =
        serde_json::from_slice(&output.stdout).expect("reference graph must be JSON");
    assert_eq!(graph["schema"], "unity-asset.reference-graph.v1");
    assert_eq!(graph["complete"], true);
    assert_eq!(graph["coverage"]["total_sources"], 1);
    assert_eq!(graph["coverage"]["scanned_sources"], 1);
    assert_eq!(graph["coverage"]["total_nodes"], 2);
    assert_eq!(graph["coverage"]["indexed_nodes"], 2);
    assert_eq!(graph["coverage"]["fact_count"], 1);
    assert_eq!(graph["projection"]["nodes_written"], 2);
    assert_eq!(graph["projection"]["facts_written"], 1);
    assert_eq!(graph["projection"]["resolved_edges_written"], 1);
    assert_eq!(graph["resolution_counts"]["resolved"], 1);
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(graph["facts"].as_array().unwrap().len(), 1);
    assert_eq!(graph["facts"][0]["resolution"]["state"], "resolved");
    assert_eq!(graph["diagnostics"].as_array().unwrap().len(), 0);
}

#[test]
fn typed_workspace_transaction_survives_independent_cli_processes() {
    let temp = tempfile::tempdir().expect("temporary workspace must be available");
    let input = write_workspace(temp.path());
    let (plan, address) = write_mutation_contracts(temp.path(), &input);

    let prepared = run(&[
        OsStr::new("workspace"),
        OsStr::new("prepare"),
        OsStr::new("--input"),
        input.as_os_str(),
        OsStr::new("--plan"),
        plan.as_os_str(),
    ]);
    assert_success(&prepared);
    let prepared: Value =
        serde_json::from_slice(&prepared.stdout).expect("prepare report must be JSON");
    assert_eq!(prepared["version"], PREPARE_REPORT_VERSION);
    assert_eq!(prepared["operation_count"], 1);
    assert!(
        fs::read_to_string(&input)
            .expect("read unmodified source")
            .contains("m_Name: Alpha")
    );

    let preview = run(&[
        OsStr::new("workspace"),
        OsStr::new("preview"),
        OsStr::new("--input"),
        input.as_os_str(),
        OsStr::new("--plan"),
        plan.as_os_str(),
        OsStr::new("--address-json"),
        address.as_os_str(),
    ]);
    assert_success(&preview);
    let preview: Value = serde_json::from_slice(&preview.stdout).expect("preview must be JSON");
    assert_eq!(preview["version"], 1);
    assert_eq!(preview["class"]["properties"]["m_Name"], "Committed");
    assert!(
        fs::read_to_string(&input)
            .expect("read unmodified source")
            .contains("m_Name: Alpha")
    );

    let committed = run(&[
        OsStr::new("workspace"),
        OsStr::new("commit"),
        OsStr::new("--input"),
        input.as_os_str(),
        OsStr::new("--plan"),
        plan.as_os_str(),
        OsStr::new("--publication-root"),
        temp.path().as_os_str(),
    ]);
    assert_success(&committed);
    let committed: Value =
        serde_json::from_slice(&committed.stdout).expect("commit report must be JSON");
    assert_eq!(committed["version"], COMMIT_REPORT_VERSION);
    assert_eq!(
        committed["changes"]["changed_objects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        fs::read_to_string(&input)
            .expect("read committed source")
            .contains("m_Name: Committed")
    );

    let reopened = run(&[
        OsStr::new("workspace"),
        OsStr::new("inspect"),
        OsStr::new("object"),
        OsStr::new("--input"),
        input.as_os_str(),
        OsStr::new("--address-json"),
        address.as_os_str(),
    ]);
    assert_success(&reopened);
    let reopened: Value =
        serde_json::from_slice(&reopened.stdout).expect("reopened inspection must be JSON");
    assert_eq!(reopened["class"]["properties"]["m_Name"], "Committed");

    let discovered = run(&[
        OsStr::new("workspace"),
        OsStr::new("recover"),
        OsStr::new("discover"),
        OsStr::new("--publication-root"),
        temp.path().as_os_str(),
    ]);
    assert_success(&discovered);
    let discovered: Value =
        serde_json::from_slice(&discovered.stdout).expect("recovery inventory must be JSON");
    assert_eq!(discovered["version"], RECOVERY_DISCOVERY_VERSION);
    assert_eq!(discovered["recoveries"].as_array().unwrap().len(), 1);
    assert_eq!(
        discovered["recoveries"][0]["transaction"],
        committed["transaction"]
    );

    let locator = temp.path().join("recovery-locator.json");
    fs::write(
        &locator,
        serde_json::to_vec(&committed["recovery"]).expect("serialize recovery locator"),
    )
    .expect("write recovery locator");
    truncate_recovery_events_after(&committed["recovery"], "published");

    let recovered = run(&[
        OsStr::new("workspace"),
        OsStr::new("recover"),
        OsStr::new("resume"),
        OsStr::new("--locator-json"),
        locator.as_os_str(),
    ]);
    assert_success(&recovered);
    let recovered: Value =
        serde_json::from_slice(&recovered.stdout).expect("recovery outcome must be JSON");
    assert_eq!(recovered["version"], RECOVERY_OUTCOME_VERSION);
    assert_eq!(recovered["outcome"]["status"], "filesystem_recovered");
    assert_eq!(
        recovered["outcome"]["report"]["transaction"],
        committed["transaction"]
    );

    let finalized = run(&[
        OsStr::new("workspace"),
        OsStr::new("recover"),
        OsStr::new("finalize"),
        OsStr::new("--input"),
        input.as_os_str(),
        OsStr::new("--locator-json"),
        locator.as_os_str(),
    ]);
    assert_success(&finalized);
    let finalized: Value =
        serde_json::from_slice(&finalized.stdout).expect("finalized recovery must be JSON");
    assert_eq!(finalized["version"], RECOVERY_OUTCOME_VERSION);
    assert_eq!(finalized["outcome"]["status"], "finalized");
    assert_eq!(
        finalized["outcome"]["report"]["transaction"],
        committed["transaction"]
    );

    let replayed = run(&[
        OsStr::new("workspace"),
        OsStr::new("recover"),
        OsStr::new("resume"),
        OsStr::new("--locator-json"),
        locator.as_os_str(),
    ]);
    assert_success(&replayed);
    let replayed: Value =
        serde_json::from_slice(&replayed.stdout).expect("replayed recovery must be JSON");
    assert_eq!(replayed["outcome"]["status"], "historical_commit_receipt");
    assert_eq!(
        replayed["outcome"]["report"]["transaction"],
        committed["transaction"]
    );
}

#[test]
fn interrupted_cli_commit_can_be_abandoned_and_replayed_idempotently() {
    let temp = tempfile::tempdir().expect("temporary workspace must be available");
    let input = write_workspace(temp.path());
    let (plan, _) = write_mutation_contracts(temp.path(), &input);
    let committed = run(&[
        OsStr::new("workspace"),
        OsStr::new("commit"),
        OsStr::new("--input"),
        input.as_os_str(),
        OsStr::new("--plan"),
        plan.as_os_str(),
        OsStr::new("--publication-root"),
        temp.path().as_os_str(),
    ]);
    assert_success(&committed);
    let committed: Value =
        serde_json::from_slice(&committed.stdout).expect("commit report must be JSON");
    assert!(
        fs::read_to_string(&input)
            .expect("read committed source")
            .contains("m_Name: Committed")
    );

    let locator = temp.path().join("abandon-recovery-locator.json");
    fs::write(
        &locator,
        serde_json::to_vec(&committed["recovery"]).expect("serialize recovery locator"),
    )
    .expect("write recovery locator");
    truncate_recovery_events_after(&committed["recovery"], "promotion_intent");

    let abandoned = run(&[
        OsStr::new("workspace"),
        OsStr::new("recover"),
        OsStr::new("abandon"),
        OsStr::new("--locator-json"),
        locator.as_os_str(),
    ]);
    assert_success(&abandoned);
    let abandoned: Value =
        serde_json::from_slice(&abandoned.stdout).expect("rollback outcome must be JSON");
    assert_eq!(abandoned["version"], RECOVERY_OUTCOME_VERSION);
    assert_eq!(abandoned["outcome"]["status"], "rolled_back");
    assert!(
        fs::read_to_string(&input)
            .expect("read restored source")
            .contains("m_Name: Alpha")
    );

    let replayed = run(&[
        OsStr::new("workspace"),
        OsStr::new("recover"),
        OsStr::new("abandon"),
        OsStr::new("--locator-json"),
        locator.as_os_str(),
    ]);
    assert_success(&replayed);
    let replayed: Value =
        serde_json::from_slice(&replayed.stdout).expect("replayed rollback must be JSON");
    assert_eq!(replayed["outcome"]["status"], "historical_rollback_receipt");
    assert!(
        fs::read_to_string(&input)
            .expect("read idempotently restored source")
            .contains("m_Name: Alpha")
    );
}
