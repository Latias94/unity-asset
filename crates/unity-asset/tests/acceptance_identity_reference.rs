use std::fs;
use std::path::Path;

use unity_asset::reference::{
    RawReferenceTarget, ReferenceDirection, ReferenceGraphBuildOptions, ReferenceResolution,
    ReferenceTraversalLimits,
};
use unity_asset::schema::SchemaRecipePlanner;
use unity_asset::workspace::{
    AssetWorkspace, GenericMutation, MutationPlanBuilder, PrepareOptions, ReferenceTarget,
    SourceOpenRequest, WorkspaceByteOrder, WorkspaceInspector, WorkspaceLookup,
    WorkspaceObjectFormatInspection, WorkspaceObjectInspection, WorkspaceSourceFormatInspection,
    WorkspaceSourceInspection, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, FieldPath, ObjectAddress, RevisionedObjectHandle, SourceAlias, SourceId,
    SourceKind, SourceLocator,
};

#[path = "support/scalar_fixture.rs"]
mod scalar_fixture;

const SCALAR_WIRE_V22: &[u8] =
    include_bytes!("../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin");
const TRANSFORM_V22: &[u8] = include_bytes!(
    "../../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin"
);
const ORIGINAL_SCALAR_PATH_ID: i64 = 42;
const SHARED_SIGNED_PATH_ID: i64 = -7;
const ORIGINAL_SCALAR_VALUE: i32 = 0x16AA_BBCC;
const IDENTITY_A_VALUE: i32 = 101;
const IDENTITY_B_VALUE: i32 = 202;
const IDENTITY_A_ALIAS: &str = "identity-a.assets";
const IDENTITY_B_ALIAS: &str = "identity-b.assets";
const PREPARED_OWNER_ALIAS: &str = "prepared-owner.assets";
const PREPARED_TARGET_ALIAS: &str = "prepared-target.assets";

fn scalar_fixture(path_id: i64, value: i32) -> Vec<u8> {
    scalar_fixture::record_scalar_v22(SCALAR_WIRE_V22, path_id, value)
}

fn load_serialized_source(workspace: &mut AssetWorkspace, path: &Path, alias: &str) -> SourceId {
    workspace
        .load_source(
            SourceOpenRequest::new(path, SourceAlias::new(alias).unwrap())
                .with_kind_hint(SourceKind::SerializedFile),
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
}

fn binary_address(alias: &str, path_id: i64) -> ObjectAddress {
    ObjectAddress::binary_direct(SourceLocator::path(alias).unwrap(), path_id).unwrap()
}

fn resolved_handle(view: &impl WorkspaceView, address: &ObjectAddress) -> RevisionedObjectHandle {
    let WorkspaceLookup::Resolved(handle) = view
        .resolve_object(address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("expected a resolved object lookup");
    };
    handle
}

fn inspected_object(
    view: &impl WorkspaceView,
    address: &ObjectAddress,
) -> WorkspaceObjectInspection {
    let WorkspaceLookup::Resolved(object) = WorkspaceInspector::new(view)
        .object(address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("expected a resolved object inspection");
    };
    object
}

fn inspected_source(view: &impl WorkspaceView, source: SourceId) -> WorkspaceSourceInspection {
    let WorkspaceLookup::Resolved(source) = WorkspaceInspector::new(view)
        .source(source, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("expected a resolved source inspection");
    };
    source
}

fn read_i64(view: &impl WorkspaceView, handle: &RevisionedObjectHandle, path: &FieldPath) -> i64 {
    view.read_object(handle, &mut AssetLoadBudget::default())
        .unwrap()
        .class()
        .value_at_path(path)
        .unwrap()
        .as_i64()
        .unwrap()
}

fn read_binary_pptr(
    view: &impl WorkspaceView,
    handle: &RevisionedObjectHandle,
    path: &FieldPath,
) -> (i64, i64) {
    let file_id = path.clone().push_field("m_FileID").unwrap();
    let path_id = path.clone().push_field("m_PathID").unwrap();
    (
        read_i64(view, handle, &file_id),
        read_i64(view, handle, &path_id),
    )
}

fn assert_signed_scalar_source(
    source: &WorkspaceSourceInspection,
    expected_id: SourceId,
    expected_locator: &SourceLocator,
    expected_revision: unity_asset::WorkspaceRevision,
    expected_length: usize,
) {
    assert_eq!(source.revision(), expected_revision);
    assert_eq!(source.source().id(), expected_id);
    assert_eq!(source.source().id().workspace(), expected_id.workspace());
    assert_eq!(source.source().kind(), SourceKind::SerializedFile);
    assert_eq!(source.source().locator(), expected_locator);
    assert_eq!(source.source().parent(), None);
    assert_eq!(source.encoded_length(), expected_length as u64);
    let WorkspaceSourceFormatInspection::SerializedFile(summary) = source.format() else {
        panic!("expected SerializedFile source metadata");
    };
    assert_eq!(summary.version(), 22);
    assert_eq!(summary.byte_order(), WorkspaceByteOrder::Big);
    assert_eq!(summary.object_count(), 1);
    let path_ids = summary.path_ids();
    assert_eq!(path_ids.negative(), 1);
    assert_eq!(path_ids.positive(), 0);
    assert_eq!(path_ids.minimum(), Some(SHARED_SIGNED_PATH_ID));
    assert_eq!(path_ids.maximum(), Some(SHARED_SIGNED_PATH_ID));
}

#[test]
fn same_signed_path_id_is_scoped_by_serialized_source_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path_a = directory.path().join(IDENTITY_A_ALIAS);
    let path_b = directory.path().join(IDENTITY_B_ALIAS);
    let bytes_a = scalar_fixture(SHARED_SIGNED_PATH_ID, IDENTITY_A_VALUE);
    let bytes_b = scalar_fixture(SHARED_SIGNED_PATH_ID, IDENTITY_B_VALUE);
    fs::write(&path_a, &bytes_a).unwrap();
    fs::write(&path_b, &bytes_b).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    let source_a = load_serialized_source(&mut workspace, &path_a, IDENTITY_A_ALIAS);
    let source_b = load_serialized_source(&mut workspace, &path_b, IDENTITY_B_ALIAS);
    assert_ne!(source_a, source_b);
    assert_eq!(source_a.workspace(), workspace.workspace_id());
    assert_eq!(source_b.workspace(), workspace.workspace_id());

    let snapshot = workspace.snapshot();
    assert_eq!(snapshot.workspace_id(), workspace.workspace_id());
    assert_eq!(snapshot.revision(), workspace.revision());
    let locator_a = SourceLocator::path(IDENTITY_A_ALIAS).unwrap();
    let locator_b = SourceLocator::path(IDENTITY_B_ALIAS).unwrap();
    assert_signed_scalar_source(
        &inspected_source(&snapshot, source_a),
        source_a,
        &locator_a,
        snapshot.revision(),
        bytes_a.len(),
    );
    assert_signed_scalar_source(
        &inspected_source(&snapshot, source_b),
        source_b,
        &locator_b,
        snapshot.revision(),
        bytes_b.len(),
    );

    let address_a = binary_address(IDENTITY_A_ALIAS, SHARED_SIGNED_PATH_ID);
    let address_b = binary_address(IDENTITY_B_ALIAS, SHARED_SIGNED_PATH_ID);
    let object_a = inspected_object(&snapshot, &address_a);
    let object_b = inspected_object(&snapshot, &address_b);
    assert_eq!(object_a.address(), &address_a);
    assert_eq!(object_b.address(), &address_b);
    assert!(matches!(
        object_a.format(),
        WorkspaceObjectFormatInspection::Binary {
            path_id: SHARED_SIGNED_PATH_ID,
            byte_size: 4,
            payload_bytes: 4,
            byte_order: WorkspaceByteOrder::Big,
            ..
        }
    ));
    assert!(matches!(
        object_b.format(),
        WorkspaceObjectFormatInspection::Binary {
            path_id: SHARED_SIGNED_PATH_ID,
            byte_size: 4,
            payload_bytes: 4,
            byte_order: WorkspaceByteOrder::Big,
            ..
        }
    ));

    let handle_a = object_a.object().handle().clone();
    let handle_b = object_b.object().handle().clone();
    assert_ne!(handle_a, handle_b);
    assert_ne!(handle_a.object(), handle_b.object());
    assert_eq!(handle_a.workspace(), snapshot.workspace_id());
    assert_eq!(handle_b.workspace(), snapshot.workspace_id());
    assert_eq!(handle_a.revision(), snapshot.revision());
    assert_eq!(handle_b.revision(), snapshot.revision());
    assert_eq!(handle_a.object().source(), source_a);
    assert_eq!(handle_b.object().source(), source_b);
    assert_eq!(
        handle_a.object().binary_path_id(),
        Some(SHARED_SIGNED_PATH_ID)
    );
    assert_eq!(
        handle_b.object().binary_path_id(),
        Some(SHARED_SIGNED_PATH_ID)
    );
    assert_eq!(
        snapshot
            .object_address(&handle_a, &mut AssetLoadBudget::default())
            .unwrap(),
        address_a
    );
    assert_eq!(
        snapshot
            .object_address(&handle_b, &mut AssetLoadBudget::default())
            .unwrap(),
        address_b
    );

    let value_path = FieldPath::root().push_field("m_Value").unwrap();
    assert_eq!(
        object_a
            .object()
            .class()
            .value_at_path(&value_path)
            .unwrap()
            .as_i64(),
        Some(i64::from(IDENTITY_A_VALUE))
    );
    assert_eq!(
        object_b
            .object()
            .class()
            .value_at_path(&value_path)
            .unwrap()
            .as_i64(),
        Some(i64::from(IDENTITY_B_VALUE))
    );
    assert_eq!(
        read_i64(&snapshot, &handle_a, &value_path),
        i64::from(IDENTITY_A_VALUE)
    );
    assert_eq!(
        read_i64(&snapshot, &handle_b, &value_path),
        i64::from(IDENTITY_B_VALUE)
    );

    let inspector = WorkspaceInspector::new(&snapshot);
    assert_eq!(
        inspector
            .sources(&mut AssetLoadBudget::default())
            .unwrap()
            .len(),
        2
    );
    let identities = inspector
        .objects(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .map(|object| object.object().handle().object().clone())
        .collect::<Vec<_>>();
    assert_eq!(identities.len(), 2);
    assert!(identities.contains(handle_a.object()));
    assert!(identities.contains(handle_b.object()));
}

#[test]
fn prepared_cross_source_reference_is_coherent_across_object_and_graph_queries() {
    let directory = tempfile::tempdir().unwrap();
    let owner_path = directory.path().join(PREPARED_OWNER_ALIAS);
    let target_path = directory.path().join(PREPARED_TARGET_ALIAS);
    fs::write(&owner_path, TRANSFORM_V22).unwrap();
    fs::write(
        &target_path,
        scalar_fixture(ORIGINAL_SCALAR_PATH_ID, ORIGINAL_SCALAR_VALUE),
    )
    .unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    let owner_source = load_serialized_source(&mut workspace, &owner_path, PREPARED_OWNER_ALIAS);
    let target_source = load_serialized_source(&mut workspace, &target_path, PREPARED_TARGET_ALIAS);
    assert_ne!(owner_source, target_source);

    let owner_parent_address = binary_address(PREPARED_OWNER_ALIAS, 1);
    let owner_child_address = binary_address(PREPARED_OWNER_ALIAS, 2);
    let target_address = binary_address(PREPARED_TARGET_ALIAS, ORIGINAL_SCALAR_PATH_ID);
    let father_path = FieldPath::root().push_field("m_Father").unwrap();
    let scalar_value_path = FieldPath::root().push_field("m_Value").unwrap();
    let base = workspace.snapshot();
    let base_owner_parent = resolved_handle(&base, &owner_parent_address);
    let base_owner_child = resolved_handle(&base, &owner_child_address);
    let base_target = resolved_handle(&base, &target_address);
    assert_eq!(base_owner_parent.object().source(), owner_source);
    assert_eq!(base_owner_child.object().source(), owner_source);
    assert_eq!(base_target.object().source(), target_source);
    assert_eq!(base_owner_parent.revision(), base.revision());
    assert_eq!(base_owner_child.revision(), base.revision());
    assert_eq!(base_target.revision(), base.revision());
    assert_eq!(
        read_binary_pptr(&base, &base_owner_child, &father_path),
        (0, 1)
    );
    assert_eq!(
        read_i64(&base, &base_target, &scalar_value_path),
        i64::from(ORIGINAL_SCALAR_VALUE)
    );

    let base_graph = base
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(base_graph.workspace_id(), base.workspace_id());
    assert_eq!(base_graph.revision(), base.revision());
    assert!(base_graph.is_complete());
    let base_father = base_graph
        .outgoing(&base_owner_child)
        .unwrap()
        .find(|fact| fact.field_path() == &father_path)
        .unwrap();
    let RawReferenceTarget::Binary {
        file_id,
        path_id,
        external,
    } = base_father.raw_target()
    else {
        panic!("expected a binary base PPtr");
    };
    assert_eq!((*file_id, *path_id), (0, 1));
    assert!(external.is_none());
    let ReferenceResolution::Resolved(base_resolved_parent) = base_father.resolution() else {
        panic!("expected the base PPtr to resolve");
    };
    assert_eq!(base_resolved_parent, &base_owner_parent);
    assert_eq!(
        base_graph
            .incoming(&base_owner_parent)
            .unwrap()
            .filter(|fact| {
                fact.source() == &base_owner_child && fact.field_path() == &father_path
            })
            .count(),
        1
    );
    assert_eq!(
        base_graph
            .incoming(&base_target)
            .unwrap()
            .filter(|fact| fact.field_path() == &father_path)
            .count(),
        0
    );
    let base_closure = base_graph
        .closure(
            std::slice::from_ref(&base_owner_child),
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(base_closure.workspace_id(), base.workspace_id());
    assert_eq!(base_closure.revision(), base.revision());
    assert_eq!(base_closure.direction(), ReferenceDirection::Outgoing);
    assert!(base_closure.is_complete());
    assert_eq!(base_closure.truncation(), None);
    assert_eq!(
        base_closure.nodes().collect::<Vec<_>>(),
        [&base_owner_child, &base_owner_parent]
    );

    let planner = SchemaRecipePlanner::new(&base);
    let observed_owner_child = planner
        .inspect(&owner_child_address, &mut AssetLoadBudget::default())
        .unwrap();
    let fragment = planner
        .lower_reference(
            &observed_owner_child,
            father_path.clone(),
            ReferenceTarget::object(owner_parent_address.clone()),
            ReferenceTarget::object(target_address.clone()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
        .into_fragment()
        .unwrap();
    let mut builder = MutationPlanBuilder::new(base.workspace_id(), base.revision());
    builder.append(fragment).unwrap();
    let plan = builder.build().unwrap();
    assert_eq!(plan.workspace_id(), base.workspace_id());
    assert_eq!(plan.base_revision(), base.revision());
    let [operation] = plan.operations() else {
        panic!("expected exactly one prepared operation");
    };
    let GenericMutation::ReferenceReplace {
        target,
        path,
        expected,
        replacement,
        ..
    } = operation.action()
    else {
        panic!("expected a reference replacement operation");
    };
    assert_eq!(target, &owner_child_address);
    assert_eq!(path, &father_path);
    assert_eq!(
        expected,
        &ReferenceTarget::object(owner_parent_address.clone())
    );
    assert_eq!(
        replacement,
        &ReferenceTarget::object(target_address.clone())
    );

    let prepared = workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let view = prepared.view();
    assert_eq!(view.workspace_id(), base.workspace_id());
    assert_eq!(view.base_revision(), base.revision());
    assert_eq!(view.revision(), prepared.report().prepared_revision());
    assert_eq!(prepared.report().base_revision(), base.revision());
    assert_ne!(view.revision(), base.revision());
    assert_eq!(workspace.revision(), base.revision());
    assert_eq!(fs::read(&owner_path).unwrap(), TRANSFORM_V22);

    let staged_owner_parent = resolved_handle(&view, &owner_parent_address);
    let staged_owner_child = resolved_handle(&view, &owner_child_address);
    let staged_target = resolved_handle(&view, &target_address);
    assert_eq!(staged_owner_parent.object(), base_owner_parent.object());
    assert_eq!(staged_owner_child.object(), base_owner_child.object());
    assert_eq!(staged_target.object(), base_target.object());
    assert_ne!(staged_owner_parent, base_owner_parent);
    assert_ne!(staged_owner_child, base_owner_child);
    assert_ne!(staged_target, base_target);
    assert_eq!(staged_owner_parent.revision(), view.revision());
    assert_eq!(staged_owner_child.revision(), view.revision());
    assert_eq!(staged_target.revision(), view.revision());
    assert_eq!(staged_owner_child.object().source(), owner_source);
    assert_eq!(staged_target.object().source(), target_source);
    assert_eq!(
        view.object_address(&staged_owner_child, &mut AssetLoadBudget::default())
            .unwrap(),
        owner_child_address
    );
    assert_eq!(
        view.object_address(&staged_target, &mut AssetLoadBudget::default())
            .unwrap(),
        target_address
    );
    assert_eq!(
        read_binary_pptr(&view, &staged_owner_child, &father_path),
        (1, ORIGINAL_SCALAR_PATH_ID)
    );
    assert_eq!(
        read_i64(&view, &staged_target, &scalar_value_path),
        i64::from(ORIGINAL_SCALAR_VALUE)
    );
    let inspected_staged_child = inspected_object(&view, &owner_child_address);
    assert_eq!(inspected_staged_child.address(), &owner_child_address);
    assert_eq!(
        inspected_staged_child.object().handle(),
        &staged_owner_child
    );

    let staged_graph = view.reference_graph();
    assert_eq!(staged_graph.workspace_id(), view.workspace_id());
    assert_eq!(staged_graph.revision(), view.revision());
    assert!(staged_graph.is_complete());
    assert!(
        staged_graph
            .nodes()
            .iter()
            .all(|node| node.revision() == view.revision())
    );
    assert_eq!(
        staged_graph.address(&staged_owner_child).unwrap(),
        &owner_child_address
    );
    assert_eq!(
        staged_graph.address(&staged_target).unwrap(),
        &target_address
    );
    let staged_father = staged_graph
        .outgoing(&staged_owner_child)
        .unwrap()
        .find(|fact| fact.field_path() == &father_path)
        .unwrap();
    assert_eq!(staged_father.source(), &staged_owner_child);
    let RawReferenceTarget::Binary {
        file_id,
        path_id,
        external,
    } = staged_father.raw_target()
    else {
        panic!("expected a binary staged PPtr");
    };
    assert_eq!((*file_id, *path_id), (1, ORIGINAL_SCALAR_PATH_ID));
    let external = external.as_ref().unwrap();
    assert_eq!(external.index(), 0);
    assert_eq!(external.guid(), None);
    assert_eq!(external.type_id(), 0);
    assert_eq!(external.path(), PREPARED_TARGET_ALIAS);
    let ReferenceResolution::Resolved(staged_resolved_target) = staged_father.resolution() else {
        panic!("expected the staged PPtr to resolve");
    };
    assert_eq!(staged_resolved_target, &staged_target);
    assert_eq!(staged_resolved_target.object().source(), target_source);
    assert_eq!(staged_resolved_target.revision(), view.revision());

    let incoming_target = staged_graph
        .incoming(&staged_target)
        .unwrap()
        .filter(|fact| fact.source() == &staged_owner_child && fact.field_path() == &father_path)
        .collect::<Vec<_>>();
    assert_eq!(incoming_target.len(), 1);
    assert!(matches!(
        incoming_target[0].resolution(),
        ReferenceResolution::Resolved(target) if target == &staged_target
    ));
    assert_eq!(
        staged_graph
            .incoming(&staged_owner_parent)
            .unwrap()
            .filter(|fact| {
                fact.source() == &staged_owner_child && fact.field_path() == &father_path
            })
            .count(),
        0
    );
    let staged_closure = staged_graph
        .closure(
            std::slice::from_ref(&staged_owner_child),
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(staged_closure.workspace_id(), view.workspace_id());
    assert_eq!(staged_closure.revision(), view.revision());
    assert_eq!(staged_closure.direction(), ReferenceDirection::Outgoing);
    assert!(staged_closure.is_complete());
    assert_eq!(staged_closure.truncation(), None);
    assert_eq!(
        staged_closure.nodes().collect::<Vec<_>>(),
        [&staged_owner_child, &staged_target]
    );

    assert_eq!(
        read_binary_pptr(&base, &base_owner_child, &father_path),
        (0, 1)
    );
    let base_father_after_prepare = base_graph
        .outgoing(&base_owner_child)
        .unwrap()
        .find(|fact| fact.field_path() == &father_path)
        .unwrap();
    assert!(matches!(
        base_father_after_prepare.resolution(),
        ReferenceResolution::Resolved(target) if target == &base_owner_parent
    ));
    assert_eq!(base_graph.revision(), base.revision());
    assert_ne!(base_graph.revision(), staged_graph.revision());
}
