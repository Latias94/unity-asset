use unity_asset_binary::BinaryError;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{
    TypeTree, TypeTreeNode, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeSchema,
};
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, FieldPath, FieldPathSegment,
};

fn pptr(name: &str) -> TypeTreeNode {
    pptr_with_path_type(name, "long long")
}

fn pptr_with_path_type(name: &str, path_type: &str) -> TypeTreeNode {
    let mut node = TypeTreeNode::with_info("PPtr<Object>".into(), name.into(), -1);
    node.children
        .push(TypeTreeNode::with_info("int".into(), "m_FileID".into(), -1));
    node.children.push(TypeTreeNode::with_info(
        path_type.into(),
        "m_PathID".into(),
        -1,
    ));
    node
}

fn pptr_pair_sequence(name: &str) -> TypeTreeNode {
    let mut pair = TypeTreeNode::with_info("pair".into(), "data".into(), -1);
    pair.children.push(pptr("first"));
    pair.children.push(pptr("second"));

    let mut array = TypeTreeNode::with_info("Array".into(), "Array".into(), -1);
    array
        .children
        .push(TypeTreeNode::with_info("int".into(), "size".into(), -1));
    array.children.push(pair);

    let mut vector = TypeTreeNode::with_info("vector".into(), name.into(), -1);
    vector.children.push(array);
    vector
}

fn pptr_sequence_with_path_type(name: &str, path_type: &str) -> TypeTreeNode {
    let mut array = TypeTreeNode::with_info("Array".into(), "Array".into(), -1);
    array
        .children
        .push(TypeTreeNode::with_info("int".into(), "size".into(), -1));
    array.children.push(pptr_with_path_type("data", path_type));

    let mut vector = TypeTreeNode::with_info("vector".into(), name.into(), -1);
    vector.children.push(array);
    vector
}

fn schema(root: TypeTreeNode) -> TypeTreeSchema {
    let mut tree = TypeTree::new();
    tree.add_node(root);
    TypeTreeSchema::compile(&tree, &[], &mut AssetLoadBudget::default()).unwrap()
}

fn path(segments: impl IntoIterator<Item = FieldPathSegment>) -> FieldPath {
    FieldPath::from_segments(segments.into_iter().collect()).unwrap()
}

#[test]
fn occurrences_retain_wire_order_raw_ids_and_materialized_paths() {
    let mut root = TypeTreeNode::with_info("Root".into(), "Root".into(), -1);
    root.children.push(pptr("m_First"));
    root.children.push(pptr_pair_sequence("m_Entries"));
    root.children.push(pptr("m_Last"));
    let schema = schema(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&10_i64.to_le_bytes());
    bytes.extend_from_slice(&1_i32.to_le_bytes());
    bytes.extend_from_slice(&(-2_i32).to_le_bytes());
    bytes.extend_from_slice(&20_i64.to_le_bytes());
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&0_i64.to_le_bytes());
    bytes.extend_from_slice(&3_i32.to_le_bytes());
    bytes.extend_from_slice(&30_i64.to_le_bytes());

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let scan = schema
        .scan_reference_occurrences(&mut reader, &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(reader.position() as usize, bytes.len());
    assert!(scan.diagnostics.is_empty());
    assert_eq!(scan.occurrences.len(), 4);
    assert_eq!(
        scan.occurrences[0].field_path,
        path([FieldPathSegment::field("m_First").unwrap()])
    );
    assert_eq!(
        (scan.occurrences[0].file_id, scan.occurrences[0].path_id),
        (0, 10)
    );
    assert_eq!(
        scan.occurrences[1].field_path,
        path([
            FieldPathSegment::field("m_Entries").unwrap(),
            FieldPathSegment::Index(0),
            FieldPathSegment::Index(0),
        ])
    );
    assert_eq!(
        (scan.occurrences[1].file_id, scan.occurrences[1].path_id),
        (-2, 20)
    );
    assert_eq!(
        scan.occurrences[2].field_path,
        path([
            FieldPathSegment::field("m_Entries").unwrap(),
            FieldPathSegment::Index(0),
            FieldPathSegment::Index(1),
        ])
    );
    assert_eq!(
        (scan.occurrences[2].file_id, scan.occurrences[2].path_id),
        (0, 0)
    );
    assert_eq!(
        scan.occurrences[3].field_path,
        path([FieldPathSegment::field("m_Last").unwrap()])
    );
    assert_eq!(
        (scan.occurrences[3].file_id, scan.occurrences[3].path_id),
        (3, 30)
    );
    assert_eq!(scan.stats.pptrs_emitted, 3);
    assert_eq!(scan.stats.unity_values_materialized, 0);
}

#[test]
fn legacy_pptr_scan_is_a_non_null_projection_of_occurrences() {
    let mut root = TypeTreeNode::with_info("Root".into(), "Root".into(), -1);
    root.children.push(pptr("m_Internal"));
    root.children.push(pptr("m_Null"));
    root.children.push(pptr("m_External"));
    let schema = schema(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&10_i64.to_le_bytes());
    bytes.extend_from_slice(&7_i32.to_le_bytes());
    bytes.extend_from_slice(&0_i64.to_le_bytes());
    bytes.extend_from_slice(&(-4_i32).to_le_bytes());
    bytes.extend_from_slice(&30_i64.to_le_bytes());

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let projected = schema
        .scan_pptrs(&mut reader, &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(projected.internal, vec![10]);
    assert_eq!(projected.external, vec![(-4, 30)]);
    assert_eq!(projected.stats.pptrs_emitted, 2);
    assert_eq!(projected.stats.unity_values_materialized, 0);
}

#[test]
fn unnamed_record_children_keep_distinct_ordinal_paths() {
    let mut root = TypeTreeNode::with_info("Root".into(), "Root".into(), -1);
    root.children.push(pptr(""));
    root.children.push(pptr(""));
    let schema = schema(root);

    let mut bytes = Vec::new();
    for _ in 0..2 {
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.extend_from_slice(&42_i64.to_le_bytes());
    }
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let scan = schema
        .scan_reference_occurrences(&mut reader, &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(scan.occurrences.len(), 2);
    assert_eq!(
        scan.occurrences[0].field_path,
        path([FieldPathSegment::Index(0)])
    );
    assert_eq!(
        scan.occurrences[1].field_path,
        path([FieldPathSegment::Index(1)])
    );
    assert_eq!(scan.occurrences[0].path_id, 42);
    assert_eq!(scan.occurrences[1].path_id, 42);
}

#[test]
fn nested_pptrs_follow_depth_first_completion_order() {
    let mut outer = pptr("m_Outer");
    outer.children.push(pptr("m_Nested"));
    let mut root = TypeTreeNode::with_info("Root".into(), "Root".into(), -1);
    root.children.push(outer);
    let schema = schema(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&10_i64.to_le_bytes());
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&20_i64.to_le_bytes());
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let scan = schema
        .scan_reference_occurrences(&mut reader, &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(scan.occurrences.len(), 2);
    assert_eq!(
        scan.occurrences[0].field_path,
        path([
            FieldPathSegment::field("m_Outer").unwrap(),
            FieldPathSegment::field("m_Nested").unwrap(),
        ])
    );
    assert_eq!(scan.occurrences[0].path_id, 20);
    assert_eq!(
        scan.occurrences[1].field_path,
        path([FieldPathSegment::field("m_Outer").unwrap()])
    );
    assert_eq!(scan.occurrences[1].path_id, 10);
}

#[test]
fn legacy_null_scan_keeps_its_wire_only_tight_budget() {
    let mut root = TypeTreeNode::with_info("Root".into(), "Root".into(), -1);
    root.children.push(pptr("m_Null"));
    let schema = schema(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&7_i32.to_le_bytes());
    bytes.extend_from_slice(&0_i64.to_le_bytes());
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: u64::try_from(bytes.len()).unwrap(),
        max_entries: 4,
        max_members: 3,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let scan = schema.scan_pptrs(&mut reader, &mut budget).unwrap();

    assert!(scan.internal.is_empty());
    assert!(scan.external.is_empty());
    assert_eq!(scan.stats.owned_bytes, 0);
    assert_eq!(reader.position() as usize, bytes.len());
}

#[test]
fn lenient_diagnostic_carries_the_complete_failed_field_path() {
    let mut root = TypeTreeNode::with_info("Root".into(), "Root".into(), -1);
    root.children
        .push(pptr_with_path_type("m_Broken", "UInt64"));
    root.children.push(pptr("m_After"));
    let schema = schema(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&99_i64.to_le_bytes());
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let scan = schema
        .scan_reference_occurrences_with_options(
            &mut reader,
            &mut AssetLoadBudget::default(),
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
        .unwrap();

    assert_eq!(scan.occurrences.len(), 1);
    assert_eq!(
        scan.occurrences[0].field_path,
        path([FieldPathSegment::field("m_After").unwrap()])
    );
    assert_eq!(scan.occurrences[0].path_id, 99);
    assert_eq!(scan.diagnostics.len(), 1);
    assert_eq!(
        scan.diagnostics[0].field_path,
        path([
            FieldPathSegment::field("m_Broken").unwrap(),
            FieldPathSegment::field("m_PathID").unwrap(),
        ])
    );
    assert!(scan.diagnostics[0].message.contains("does not fit in i64"));
    assert_eq!(scan.stats.unity_values_materialized, 0);
}

#[test]
fn recovered_sequence_path_does_not_pollute_the_next_sibling() {
    let mut root = TypeTreeNode::with_info("Root".into(), "Root".into(), -1);
    root.children
        .push(pptr_sequence_with_path_type("m_BrokenList", "UInt64"));
    root.children.push(pptr("m_After"));
    let schema = schema(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1_i32.to_le_bytes());
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&99_i64.to_le_bytes());
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let scan = schema
        .scan_reference_occurrences_with_options(
            &mut reader,
            &mut AssetLoadBudget::default(),
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
        .unwrap();

    assert_eq!(scan.diagnostics.len(), 1);
    assert_eq!(
        scan.diagnostics[0].field_path,
        path([
            FieldPathSegment::field("m_BrokenList").unwrap(),
            FieldPathSegment::Index(0),
            FieldPathSegment::field("m_PathID").unwrap(),
        ])
    );
    assert_eq!(scan.occurrences.len(), 1);
    assert_eq!(
        scan.occurrences[0].field_path,
        path([FieldPathSegment::field("m_After").unwrap()])
    );
    assert_eq!(scan.occurrences[0].path_id, 99);
}

#[test]
fn retained_occurrence_paths_are_charged_to_the_caller_budget() {
    let mut root = TypeTreeNode::with_info("Root".into(), "Root".into(), -1);
    root.children.push(pptr("m_Target"));
    let schema = schema(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&42_i64.to_le_bytes());
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: u64::try_from(bytes.len()).unwrap(),
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = schema
        .scan_reference_occurrences(&mut reader, &mut budget)
        .expect_err("the wire-only byte allowance must not fund retained output paths");

    assert!(matches!(
        error,
        BinaryError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        })
    ));
    assert_eq!(reader.position() as usize, bytes.len());
}
