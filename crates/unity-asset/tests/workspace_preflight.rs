use std::ffi::OsString;
use std::fs;
use std::path::Path;

use tempfile::TempDir;
use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationValue, PrepareOptions,
    PrepareStage, SourceExpectation, SourceOpenRequest, WorkspaceLookup, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, FieldPath, ObjectAddress, SourceAlias, SourceFingerprint, SourceKind,
    SourceLocator, UnityValue,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};

const SOURCE_ALIAS: &str = "read-your-writes.prefab";
const YAML: &str =
    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";

fn workspace_fixture() -> (TempDir, std::path::PathBuf, AssetWorkspace) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SOURCE_ALIAS);
    fs::write(&path, YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(SOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    (directory, path, workspace)
}

fn address() -> ObjectAddress {
    ObjectAddress::yaml(SourceLocator::path(SOURCE_ALIAS).unwrap(), "1").unwrap()
}

fn name_path() -> FieldPath {
    FieldPath::root().push_field("m_Name").unwrap()
}

fn read_name(view: &impl WorkspaceView) -> String {
    let mut budget = AssetLoadBudget::default();
    let WorkspaceLookup::Resolved(handle) = view.resolve_object(&address(), &mut budget).unwrap()
    else {
        panic!("fixture object must resolve");
    };
    let object = view.read_object(&handle, &mut budget).unwrap();
    object
        .class()
        .value_at_path(&name_path())
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

fn guard_for(value: &str) -> FieldGuard {
    let class = unity_asset::UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
    let path = name_path();
    let value = UnityValue::String(value.to_owned());
    let mut budget = AssetLoadBudget::default();
    FieldGuard::new(
        yaml_field_schema_digest(&class, &path, &value, &mut budget).unwrap(),
        semantic_value_digest(&value, &mut budget).unwrap(),
    )
}

fn plan(workspace: &AssetWorkspace, actions: Vec<GenericMutation>) -> MutationPlan {
    MutationPlan::new(
        workspace.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(SOURCE_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, YAML.as_bytes()),
        )],
        Vec::new(),
        actions,
    )
    .unwrap()
}

fn replace(guard: FieldGuard, value: &str) -> GenericMutation {
    GenericMutation::FieldReplace {
        target: address(),
        path: name_path(),
        guard,
        replacement: MutationValue::string(value).unwrap(),
    }
}

fn directory_entries(path: &Path) -> Vec<OsString> {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[test]
fn prepare_reads_earlier_field_replacement_and_performs_no_filesystem_write() {
    let (directory, path, workspace) = workspace_fixture();
    let before_entries = directory_entries(directory.path());
    let before_bytes = fs::read(&path).unwrap();
    let change = plan(
        &workspace,
        vec![
            replace(guard_for("Before"), "Middle"),
            replace(guard_for("Middle"), "After"),
        ],
    );

    let prepared = workspace
        .prepare(
            change,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(read_name(&prepared.view()), "After");
    assert_eq!(read_name(&workspace.snapshot()), "Before");
    assert_eq!(fs::read(&path).unwrap(), before_bytes);
    assert_eq!(directory_entries(directory.path()), before_entries);
}

#[test]
fn later_guard_failure_rolls_back_the_complete_candidate() {
    let (directory, path, workspace) = workspace_fixture();
    let before_entries = directory_entries(directory.path());
    let before_bytes = fs::read(&path).unwrap();
    let change = plan(
        &workspace,
        vec![
            replace(guard_for("Before"), "Middle"),
            replace(guard_for("Before"), "After"),
        ],
    );

    let error = workspace
        .prepare(
            change,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert_eq!(error.report().diagnostics().len(), 1);
    assert_eq!(error.report().diagnostics()[0].ordinal(), Some(1));
    assert_eq!(
        error.report().diagnostics()[0].diagnostic().code(),
        "PREPARE_MUTATION_REJECTED"
    );
    assert_eq!(read_name(&workspace.snapshot()), "Before");
    assert_eq!(fs::read(&path).unwrap(), before_bytes);
    assert_eq!(directory_entries(directory.path()), before_entries);
}

#[test]
fn missing_object_is_reported_during_address_resolution() {
    let (_directory, _path, workspace) = workspace_fixture();
    let missing = ObjectAddress::yaml(SourceLocator::path(SOURCE_ALIAS).unwrap(), "999").unwrap();
    let change = plan(
        &workspace,
        vec![GenericMutation::FieldReplace {
            target: missing.clone(),
            path: name_path(),
            guard: guard_for("Before"),
            replacement: MutationValue::string("After").unwrap(),
        }],
    );

    let error = workspace
        .prepare(
            change,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    let diagnostic = &error.report().diagnostics()[0];
    assert_eq!(diagnostic.ordinal(), Some(0));
    assert_eq!(diagnostic.stage(), PrepareStage::AddressResolution);
    assert_eq!(diagnostic.diagnostic().code(), "PREPARE_ADDRESS_MISSING");
    assert_eq!(diagnostic.diagnostic().address(), Some(&missing));
}
