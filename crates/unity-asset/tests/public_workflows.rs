use std::fs;
use std::path::Path;

use unity_asset::schema::SchemaRecipePlanner;
use unity_asset::workspace::{
    AssetWorkspace, MutationPlanBuilder, MutationValue, PrepareOptions, PublicationTarget,
    SourceOpenRequest, WorkspaceInspector, WorkspaceLookup, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, FieldPath, ObjectAddress, ObjectId, SourceAlias, SourceKind, SourceLocator,
    UnityValue,
};

#[path = "support/scalar_fixture.rs"]
mod scalar_fixture;

const YAML_ALIAS: &str = "public-workflow.prefab";
const YAML_SOURCE: &[u8] =
    b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";
const BINARY_ALIAS: &str = "public-workflow.assets";
const BINARY_WIRE_SOURCE: &[u8] =
    include_bytes!("../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin");

fn load_workspace(path: &Path, alias: &str, kind: SourceKind) -> AssetWorkspace {
    let mut workspace = AssetWorkspace::new().expect("workspace");
    load_workspace_source(&mut workspace, path, alias, kind);
    workspace
}

fn load_workspace_source(
    workspace: &mut AssetWorkspace,
    path: &Path,
    alias: &str,
    kind: SourceKind,
) {
    workspace
        .load_source(
            SourceOpenRequest::new(path, SourceAlias::new(alias).expect("source alias"))
                .with_kind_hint(kind),
            &mut AssetLoadBudget::default(),
        )
        .expect("load source");
}

fn read_value(view: &impl WorkspaceView, address: &ObjectAddress, path: &FieldPath) -> UnityValue {
    let mut budget = AssetLoadBudget::default();
    let WorkspaceLookup::Resolved(handle) = view
        .resolve_object(address, &mut budget)
        .expect("resolve object")
    else {
        panic!("fixture object must resolve");
    };
    view.read_object(&handle, &mut budget)
        .expect("read object")
        .class()
        .value_at_path(path)
        .expect("read field")
        .clone()
}

fn resolve_object_id(view: &impl WorkspaceView, address: &ObjectAddress) -> ObjectId {
    let WorkspaceLookup::Resolved(handle) = view
        .resolve_object(address, &mut AssetLoadBudget::default())
        .expect("resolve object")
    else {
        panic!("fixture object must resolve");
    };
    handle.into_object()
}

fn prepare_field_replace(
    workspace: &AssetWorkspace,
    address: &ObjectAddress,
    path: FieldPath,
    replacement: MutationValue,
) -> unity_asset::workspace::PreparedChange {
    let snapshot = workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let observed = planner
        .inspect(address, &mut AssetLoadBudget::default())
        .expect("inspect recipe object");
    let fragment = planner
        .lower_field_replace(
            &observed,
            path,
            replacement,
            &mut AssetLoadBudget::default(),
        )
        .expect("lower field replacement");
    let mut builder = MutationPlanBuilder::new(snapshot.workspace_id(), snapshot.revision());
    builder.append(fragment).expect("append plan fragment");
    workspace
        .prepare(
            builder.build().expect("build mutation plan"),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .expect("prepare mutation")
}

fn assert_inspection(view: &impl WorkspaceView, address: &ObjectAddress) {
    let mut budget = AssetLoadBudget::default();
    let inspector = WorkspaceInspector::new(view);
    assert_eq!(
        inspector
            .sources(&mut budget)
            .expect("inspect sources")
            .len(),
        1
    );
    assert_eq!(
        inspector
            .objects(&mut budget)
            .expect("inspect objects")
            .len(),
        1
    );
    assert!(matches!(
        inspector
            .object(address, &mut budget)
            .expect("inspect object"),
        WorkspaceLookup::Resolved(_)
    ));
}

#[test]
fn yaml_public_workflow_previews_commits_reopens_and_verifies() {
    let directory = tempfile::tempdir().expect("temporary project");
    let path = directory.path().join(YAML_ALIAS);
    fs::write(&path, YAML_SOURCE).expect("write YAML fixture");

    let mut workspace = load_workspace(&path, YAML_ALIAS, SourceKind::Yaml);
    let address = ObjectAddress::yaml(SourceLocator::path(YAML_ALIAS).expect("locator"), "1")
        .expect("YAML address");
    let field = FieldPath::root().push_field("m_Name").expect("name field");
    let base = workspace.snapshot();
    assert_inspection(&base, &address);

    let prepared = prepare_field_replace(
        &workspace,
        &address,
        field.clone(),
        MutationValue::string("After").expect("replacement"),
    );
    let preview = prepared.view();
    assert_eq!(read_value(&base, &address, &field).as_str(), Some("Before"));
    assert_eq!(
        read_value(&preview, &address, &field).as_str(),
        Some("After")
    );

    workspace
        .commit(
            prepared,
            PublicationTarget::in_place(directory.path()).expect("publication target"),
            &mut AssetLoadBudget::default(),
        )
        .expect("commit YAML mutation");
    drop(workspace);

    let reopened = load_workspace(&path, YAML_ALIAS, SourceKind::Yaml);
    let snapshot = reopened.snapshot();
    assert_inspection(&snapshot, &address);
    assert_eq!(
        read_value(&snapshot, &address, &field).as_str(),
        Some("After")
    );
}

#[test]
fn serialized_file_public_workflow_previews_commits_reopens_and_verifies() {
    let directory = tempfile::tempdir().expect("temporary project");
    let path = directory.path().join(BINARY_ALIAS);
    fs::write(
        &path,
        scalar_fixture::record_scalar_v22(BINARY_WIRE_SOURCE, 42, 0x16AA_BBCC),
    )
    .expect("write SerializedFile fixture");

    let mut workspace = load_workspace(&path, BINARY_ALIAS, SourceKind::SerializedFile);
    let address =
        ObjectAddress::binary_direct(SourceLocator::path(BINARY_ALIAS).expect("locator"), 42)
            .expect("binary address");
    let field = FieldPath::root()
        .push_field("m_Value")
        .expect("value field");
    let base = workspace.snapshot();
    assert_inspection(&base, &address);
    let original = read_value(&base, &address, &field)
        .as_i64()
        .expect("integer fixture value");
    assert_ne!(original, 123);

    let prepared = prepare_field_replace(
        &workspace,
        &address,
        field.clone(),
        MutationValue::signed(123),
    );
    let preview = prepared.view();
    assert_eq!(read_value(&base, &address, &field).as_i64(), Some(original));
    assert_eq!(read_value(&preview, &address, &field).as_i64(), Some(123));

    workspace
        .commit(
            prepared,
            PublicationTarget::in_place(directory.path()).expect("publication target"),
            &mut AssetLoadBudget::default(),
        )
        .expect("commit SerializedFile mutation");
    drop(workspace);

    let reopened = load_workspace(&path, BINARY_ALIAS, SourceKind::SerializedFile);
    let snapshot = reopened.snapshot();
    assert_inspection(&snapshot, &address);
    assert_eq!(read_value(&snapshot, &address, &field).as_i64(), Some(123));
}

#[test]
fn mixed_format_public_workflow_commits_one_transaction_and_reopens_both_sources() {
    let directory = tempfile::tempdir().expect("temporary project");
    let yaml_path = directory.path().join(YAML_ALIAS);
    let binary_path = directory.path().join(BINARY_ALIAS);
    fs::write(&yaml_path, YAML_SOURCE).expect("write YAML fixture");
    fs::write(
        &binary_path,
        scalar_fixture::record_scalar_v22(BINARY_WIRE_SOURCE, 42, 0x16AA_BBCC),
    )
    .expect("write SerializedFile fixture");

    let mut workspace = AssetWorkspace::new().expect("workspace");
    load_workspace_source(&mut workspace, &yaml_path, YAML_ALIAS, SourceKind::Yaml);
    load_workspace_source(
        &mut workspace,
        &binary_path,
        BINARY_ALIAS,
        SourceKind::SerializedFile,
    );

    let yaml_address =
        ObjectAddress::yaml(SourceLocator::path(YAML_ALIAS).expect("YAML locator"), "1")
            .expect("YAML address");
    let binary_address = ObjectAddress::binary_direct(
        SourceLocator::path(BINARY_ALIAS).expect("binary locator"),
        42,
    )
    .expect("binary address");
    let yaml_field = FieldPath::root().push_field("m_Name").expect("name field");
    let binary_field = FieldPath::root()
        .push_field("m_Value")
        .expect("value field");

    let base = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&base);
    assert_eq!(
        inspector
            .sources(&mut AssetLoadBudget::default())
            .expect("inspect sources")
            .len(),
        2
    );
    assert_eq!(
        inspector
            .objects(&mut AssetLoadBudget::default())
            .expect("inspect objects")
            .len(),
        2
    );

    let yaml_object = resolve_object_id(&base, &yaml_address);
    let binary_object = resolve_object_id(&base, &binary_address);
    let mut expected_changed_objects = vec![yaml_object, binary_object];
    expected_changed_objects.sort_unstable();
    let mut expected_changed_sources = expected_changed_objects
        .iter()
        .map(ObjectId::source)
        .collect::<Vec<_>>();
    expected_changed_sources.sort_unstable();
    expected_changed_sources.dedup();
    assert_eq!(expected_changed_sources.len(), 2);

    let yaml_before = read_value(&base, &yaml_address, &yaml_field);
    let binary_before = read_value(&base, &binary_address, &binary_field)
        .as_i64()
        .expect("integer fixture value");
    assert_eq!(yaml_before.as_str(), Some("Before"));
    assert_ne!(binary_before, 456);

    let planner = SchemaRecipePlanner::new(&base);
    let observed_yaml = planner
        .inspect(&yaml_address, &mut AssetLoadBudget::default())
        .expect("inspect YAML recipe object");
    let yaml_fragment = planner
        .lower_field_replace(
            &observed_yaml,
            yaml_field.clone(),
            MutationValue::string("Mixed After").expect("YAML replacement"),
            &mut AssetLoadBudget::default(),
        )
        .expect("lower YAML field replacement");
    let observed_binary = planner
        .inspect(&binary_address, &mut AssetLoadBudget::default())
        .expect("inspect binary recipe object");
    let binary_fragment = planner
        .lower_field_replace(
            &observed_binary,
            binary_field.clone(),
            MutationValue::signed(456),
            &mut AssetLoadBudget::default(),
        )
        .expect("lower binary field replacement");

    let mut builder = MutationPlanBuilder::new(base.workspace_id(), base.revision());
    builder
        .append(yaml_fragment)
        .expect("append YAML plan fragment");
    builder
        .append(binary_fragment)
        .expect("append binary plan fragment");
    let plan = builder.build().expect("build mixed-format mutation plan");
    assert_eq!(plan.operations().len(), 2);
    assert_eq!(plan.operations()[0].ordinal(), 0);
    assert_eq!(plan.operations()[0].action().target(), &yaml_address);
    assert_eq!(plan.operations()[1].ordinal(), 1);
    assert_eq!(plan.operations()[1].action().target(), &binary_address);

    let prepared = workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .expect("prepare mixed-format mutation");
    assert_eq!(prepared.report().operation_count(), 2);
    let prepared_sources = prepared
        .report()
        .sources()
        .iter()
        .map(|source| source.source_id())
        .collect::<Vec<_>>();
    assert_eq!(
        prepared_sources.as_slice(),
        expected_changed_sources.as_slice()
    );
    let preview = prepared.view();
    let prepared_revision = preview.revision();

    assert_eq!(
        read_value(&base, &yaml_address, &yaml_field).as_str(),
        Some("Before")
    );
    assert_eq!(
        read_value(&base, &binary_address, &binary_field).as_i64(),
        Some(binary_before)
    );
    assert_eq!(
        read_value(&preview, &yaml_address, &yaml_field).as_str(),
        Some("Mixed After")
    );
    assert_eq!(
        read_value(&preview, &binary_address, &binary_field).as_i64(),
        Some(456)
    );

    let report = workspace
        .commit(
            prepared,
            PublicationTarget::in_place(directory.path()).expect("publication target"),
            &mut AssetLoadBudget::default(),
        )
        .expect("commit mixed-format mutation");
    let changes = report.changes();
    assert_eq!(changes.transaction(), report.transaction());
    assert_eq!(changes.workspace(), workspace.workspace_id());
    assert_eq!(changes.from_revision(), base.revision());
    assert_eq!(changes.to_revision(), prepared_revision);
    assert_eq!(
        changes.changed_sources(),
        expected_changed_sources.as_slice()
    );
    assert_eq!(
        changes.changed_objects(),
        expected_changed_objects.as_slice()
    );
    assert!(changes.identity_remaps().is_empty());
    let mut committed_artifact_sources = report
        .artifacts()
        .iter()
        .map(|artifact| artifact.source())
        .collect::<Vec<_>>();
    committed_artifact_sources.sort_unstable();
    assert_eq!(
        committed_artifact_sources.as_slice(),
        expected_changed_sources.as_slice()
    );

    assert_eq!(
        read_value(&base, &yaml_address, &yaml_field).as_str(),
        Some("Before")
    );
    assert_eq!(
        read_value(&base, &binary_address, &binary_field).as_i64(),
        Some(binary_before)
    );
    assert_eq!(
        read_value(&preview, &yaml_address, &yaml_field).as_str(),
        Some("Mixed After")
    );
    assert_eq!(
        read_value(&preview, &binary_address, &binary_field).as_i64(),
        Some(456)
    );
    drop(workspace);

    let mut reopened = AssetWorkspace::new().expect("reopened workspace");
    load_workspace_source(&mut reopened, &yaml_path, YAML_ALIAS, SourceKind::Yaml);
    load_workspace_source(
        &mut reopened,
        &binary_path,
        BINARY_ALIAS,
        SourceKind::SerializedFile,
    );
    let reopened = reopened.snapshot();
    assert_eq!(
        read_value(&reopened, &yaml_address, &yaml_field).as_str(),
        Some("Mixed After")
    );
    assert_eq!(
        read_value(&reopened, &binary_address, &binary_field).as_i64(),
        Some(456)
    );
}
