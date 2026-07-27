use std::fs;

use unity_asset::reference::{ReferenceDirection, ReferenceResolution, ReferenceTraversalLimits};
use unity_asset::workspace::{
    AssetWorkspace, GenericMutation, MutationPlan, PrepareOptions, ReferenceTarget,
    SourceExpectation, WorkspaceLookup, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, FieldPath, ObjectAddress, SourceFingerprint, SourceKind, SourceLocator,
};
use unity_asset_core::yaml_field_schema_digest;

const SOURCE_ALIAS: &str = "prepared-reference.prefab";
const YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &1
MonoBehaviour:
  m_Target: {fileID: 2}
--- !u!1 &2
GameObject:
  m_Name: BeforeTarget
--- !u!1 &3
GameObject:
  m_Name: AfterTarget
"#;

fn address(anchor: &str) -> ObjectAddress {
    ObjectAddress::yaml(SourceLocator::path(SOURCE_ALIAS).unwrap(), anchor).unwrap()
}

fn target_path() -> FieldPath {
    FieldPath::root().push_field("m_Target").unwrap()
}

fn resolved_handle(view: &impl WorkspaceView, anchor: &str) -> unity_asset::RevisionedObjectHandle {
    let mut budget = AssetLoadBudget::default();
    let WorkspaceLookup::Resolved(handle) =
        view.resolve_object(&address(anchor), &mut budget).unwrap()
    else {
        panic!("fixture object must resolve");
    };
    handle
}

#[test]
fn prepared_reference_is_one_revision_across_object_and_graph_queries() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join(SOURCE_ALIAS);
    fs::write(&source_path, YAML).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let base = workspace.snapshot();
    let source = resolved_handle(&base, "1");
    let source_object = base
        .read_object(&source, &mut AssetLoadBudget::default())
        .unwrap();
    let field_path = target_path();
    let current = source_object.class().value_at_path(&field_path).unwrap();
    let schema_digest = yaml_field_schema_digest(
        source_object.class(),
        &field_path,
        current,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let plan = MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(SOURCE_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, YAML.as_bytes()),
        )],
        Vec::new(),
        vec![GenericMutation::ReferenceReplace {
            target: address("1"),
            path: field_path.clone(),
            schema_digest,
            expected: ReferenceTarget::object(address("2")),
            replacement: ReferenceTarget::object(address("3")),
        }],
    )
    .unwrap();

    let prepared = workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let view = prepared.view();
    let prepared_source = resolved_handle(&view, "1");
    let old_target = resolved_handle(&view, "2");
    let new_target = resolved_handle(&view, "3");
    let revision = view.revision();
    assert_eq!(prepared_source.revision(), revision);
    assert_eq!(old_target.revision(), revision);
    assert_eq!(new_target.revision(), revision);

    let prepared_object = view
        .read_object(&prepared_source, &mut AssetLoadBudget::default())
        .unwrap();
    let file_id_path = field_path.push_field("fileID").unwrap();
    assert_eq!(
        prepared_object
            .class()
            .value_at_path(&file_id_path)
            .unwrap()
            .as_i64(),
        Some(3)
    );

    let graph = view.reference_graph();
    let outgoing = graph
        .outgoing(&prepared_source)
        .unwrap()
        .collect::<Vec<_>>();
    assert_eq!(outgoing.len(), 1);
    assert!(matches!(
        outgoing[0].resolution(),
        ReferenceResolution::Resolved(target) if target == &new_target
    ));
    assert_eq!(graph.incoming(&old_target).unwrap().count(), 0);
    assert_eq!(graph.incoming(&new_target).unwrap().count(), 1);

    let closure = graph
        .closure(
            std::slice::from_ref(&prepared_source),
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(closure.is_complete());
    assert_eq!(
        closure.nodes().collect::<Vec<_>>(),
        [&prepared_source, &new_target]
    );

    let base_source = resolved_handle(&base, "1");
    let base_graph = base
        .reference_graph(
            unity_asset::reference::ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(matches!(
        base_graph.outgoing(&base_source).unwrap().next().unwrap().resolution(),
        ReferenceResolution::Resolved(target) if target.object().yaml_anchor() == Some("2")
    ));
    assert_eq!(fs::read(&source_path).unwrap(), YAML.as_bytes());
}
