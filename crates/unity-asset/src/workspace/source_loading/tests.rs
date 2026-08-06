use std::fs;

use super::*;
use unity_asset_core::AssetLoadLimits;

fn tree_with_child() -> TypeTree {
    let mut root = TypeTreeNode::new();
    root.children.push(TypeTreeNode::new());
    TypeTree {
        nodes: vec![root],
        ..TypeTree::default()
    }
}

#[test]
fn frozen_leaf_root_uses_the_same_zero_based_depth_as_embedded_trees() {
    let tree = TypeTree {
        nodes: vec![TypeTreeNode::new()],
        ..TypeTree::default()
    };
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    account_frozen_type_tree(&tree, &mut budget, 1).unwrap();

    assert!(budget.usage().bytes > 0);
    assert_eq!(budget.usage().entries, 1);
    assert_eq!(budget.usage().max_observed_depth, 1);
}

#[test]
fn frozen_tree_rejects_child_depth_before_child_traversal_scratch() {
    let tree = tree_with_child();
    let root = &tree.nodes[0];
    let expected_bytes = size_of::<TypeTree>()
        + tree.nodes.capacity() * size_of::<TypeTreeNode>()
        + tree.string_buffer.capacity()
        + size_of::<(&TypeTreeNode, u32)>()
        + root.type_name.capacity()
        + root.name.capacity()
        + root.children.capacity() * size_of::<TypeTreeNode>();
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = account_frozen_type_tree(&tree, &mut budget, 1).unwrap_err();

    assert!(matches!(
        error,
        WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "depth",
            limit: 1,
            requested: 2,
        })
    ));
    assert_eq!(budget.usage().bytes, u64::try_from(expected_bytes).unwrap());
    assert_eq!(budget.usage().entries, 1);
    assert_eq!(budget.usage().max_observed_depth, 1);
}

#[test]
fn owned_root_image_accounts_read_verification_and_arc_backing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("payload.resource");
    fs::write(&path, b"four").unwrap();
    let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
    let arc_bytes = arc_slice_allocation_bytes::<u8>(4).unwrap();
    let exact_bytes = 8 + arc_bytes;

    let mut short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: exact_bytes - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let error = read_owned_image(&origin, &mut short).unwrap_err();
    assert!(matches!(
        error,
        WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit,
            requested,
        }) if limit == exact_bytes - 1 && requested == exact_bytes
    ));
    assert_eq!(short.usage().bytes, 0);

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: exact_bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let image = read_owned_image(&origin, &mut exact).unwrap();
    assert_eq!(image.as_ref(), b"four");
    assert_eq!(exact.usage().bytes, exact_bytes);
}

#[test]
fn second_pass_rejects_same_length_content_change() {
    let path = Path::new("same-length.resource");
    let mut changed = std::io::Cursor::new(b"five".as_slice());
    let mut budget = AssetLoadBudget::default();

    let error = verify_stable_contents(&mut changed, b"four", path, &mut budget).unwrap_err();

    assert!(matches!(
        error,
        WorkspaceError::SourceChanged { path: changed_path } if changed_path == path
    ));
    assert_eq!(budget.usage().bytes, 4);
}

#[test]
fn second_pass_classifies_truncation_as_source_change() {
    let path = Path::new("truncated.resource");
    let mut truncated = std::io::Cursor::new(b"thr".as_slice());

    let error = verify_stable_contents(
        &mut truncated,
        b"four",
        path,
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        WorkspaceError::SourceChanged { path: changed_path } if changed_path == path
    ));
}

#[test]
fn binary_adapter_resource_failures_keep_their_public_error_variants() {
    let memory = map_binary_adapter_error(BinaryAdapterError::Parse {
        source: unity_asset_binary::error::BinaryError::MemoryError("allocation failed".to_owned()),
    });
    assert!(matches!(
        memory,
        WorkspaceError::Binary(unity_asset_binary::error::BinaryError::MemoryError(message))
            if message == "allocation failed"
    ));

    let hard_limit = map_binary_adapter_error(BinaryAdapterError::MemberBinary {
        container: BinaryContainerKind::WebFile,
        wire_ordinal: 7,
        source: unity_asset_binary::error::BinaryError::ResourceLimitExceeded(
            "member limit".to_owned(),
        ),
    });
    assert!(matches!(
        hard_limit,
        WorkspaceError::BinaryMember {
            container: WorkspaceSourceContainer::WebFile,
            wire_ordinal: 7,
            source: unity_asset_binary::error::BinaryError::ResourceLimitExceeded(message),
        } if message == "member limit"
    ));
}

#[test]
fn allocation_mappers_preserve_bytes_elements_and_slots() {
    let reserve_error = || {
        Vec::<u8>::new()
            .try_reserve(usize::MAX)
            .expect_err("an impossible capacity must fail")
    };
    for (adapter_unit, expected) in [
        (
            BinaryAdapterAllocationUnit::Bytes,
            WorkspaceAllocationUnit::Bytes,
        ),
        (
            BinaryAdapterAllocationUnit::Elements,
            WorkspaceAllocationUnit::Elements,
        ),
    ] {
        let error = map_binary_adapter_error(BinaryAdapterError::Allocation {
            resource: "binary allocation",
            requested: 9,
            unit: adapter_unit,
            source: reserve_error(),
        });
        assert!(matches!(
            error,
            WorkspaceError::Allocation {
                resource: "binary allocation",
                requested: 9,
                unit,
                ..
            } if unit == expected
        ));
    }

    for (catalog_unit, expected) in [
        (
            crate::workspace::source_catalog::CatalogAllocationUnit::Bytes,
            WorkspaceAllocationUnit::Bytes,
        ),
        (
            crate::workspace::source_catalog::CatalogAllocationUnit::Elements,
            WorkspaceAllocationUnit::Elements,
        ),
        (
            crate::workspace::source_catalog::CatalogAllocationUnit::Slots,
            WorkspaceAllocationUnit::Slots,
        ),
    ] {
        let error = WorkspaceError::from(
            crate::workspace::source_catalog::CatalogError::AllocationFailed {
                resource: "catalog allocation",
                requested: 11,
                unit: catalog_unit,
                message: "allocation failed".to_owned(),
            },
        );
        assert!(matches!(
            error,
            WorkspaceError::Allocation {
                resource: "catalog allocation",
                requested: 11,
                unit,
                ..
            } if unit == expected
        ));
    }
}
