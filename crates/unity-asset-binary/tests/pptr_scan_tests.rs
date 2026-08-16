use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{
    TypeTree, TypeTreeNode, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeSchema,
};
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits};

fn make_pptr_node(name: &str) -> TypeTreeNode {
    make_pptr_node_with_path_type(name, "long long")
}

fn make_pptr_node_with_path_type(name: &str, path_type: &str) -> TypeTreeNode {
    let mut pptr = TypeTreeNode::with_info("PPtr<Object>".to_string(), name.to_string(), -1);
    pptr.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_FileID".to_string(),
        -1,
    ));
    pptr.children.push(TypeTreeNode::with_info(
        path_type.to_string(),
        "m_PathID".to_string(),
        -1,
    ));
    pptr
}

fn make_pptr_array_node(name: &str) -> TypeTreeNode {
    let mut vec_node = TypeTreeNode::with_info("vector".to_string(), name.to_string(), -1);
    let mut array_node = TypeTreeNode::with_info("Array".to_string(), "Array".to_string(), -1);

    array_node.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "size".to_string(),
        -1,
    ));
    array_node.children.push(make_pptr_node("data"));
    vec_node.children.push(array_node);
    vec_node
}

#[test]
fn scan_pptrs_finds_internal_and_external_refs() {
    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    root.children.push(make_pptr_node("m_Single"));
    root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Marker".to_string(),
        -1,
    ));
    root.children.push(make_pptr_array_node("m_List"));
    tree.add_node(root);

    // Build bytes in the same order as the TypeTree:
    // - m_Single: fileID i32 + pathID i64
    // - m_Marker: i32
    // - m_List: size i32 + elements
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&123i64.to_le_bytes());
    bytes.extend_from_slice(&42i32.to_le_bytes());
    bytes.extend_from_slice(&2i32.to_le_bytes());
    // element 0: external
    bytes.extend_from_slice(&1i32.to_le_bytes());
    bytes.extend_from_slice(&111i64.to_le_bytes());
    // element 1: internal
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&222i64.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let scan = schema.scan_pptrs(&mut reader, &mut budget).unwrap();

    assert_eq!(reader.position() as usize, bytes.len());

    let mut internal = scan.internal.clone();
    internal.sort_unstable();
    internal.dedup();
    assert_eq!(internal, vec![123, 222]);

    let mut external = scan.external.clone();
    external.sort_unstable();
    external.dedup();
    assert_eq!(external, vec![(1, 111)]);
    assert_eq!(scan.stats.unity_values_materialized, 0);
}

#[test]
fn lenient_scan_recovers_a_proven_pptr_extent_without_materialization() {
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);
    root.children
        .push(make_pptr_node_with_path_type("m_Broken", "UInt64"));
    root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Marker".to_string(),
        -1,
    ));
    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    bytes.extend_from_slice(&0x1122_3344_i32.to_le_bytes());

    let schema = TypeTreeSchema::compile(&tree, &[], &mut AssetLoadBudget::default()).unwrap();
    let mut strict_reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let strict_error = schema
        .scan_pptrs(&mut strict_reader, &mut AssetLoadBudget::default())
        .expect_err("strict scan must reject an out-of-range PPtr path ID");
    assert!(strict_error.to_string().contains("does not fit in i64"));

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let scan = schema
        .scan_pptrs_with_options(
            &mut reader,
            &mut AssetLoadBudget::default(),
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
        .unwrap();

    assert!(scan.internal.is_empty());
    assert!(scan.external.is_empty());
    assert_eq!(reader.position() as usize, bytes.len());
    assert_eq!(scan.stats.owned_bytes, 0);
    assert_eq!(scan.stats.unity_values_materialized, 0);
}

#[test]
fn lenient_scan_propagates_resource_errors() {
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);
    root.children.push(make_pptr_node("m_Target"));
    let mut tree = TypeTree::new();
    tree.add_node(root);
    let schema = TypeTreeSchema::compile(&tree, &[], &mut AssetLoadBudget::default()).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0_i32.to_le_bytes());
    bytes.extend_from_slice(&42_i64.to_le_bytes());
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let error = schema
        .scan_pptrs_with_options(
            &mut reader,
            &mut budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
        .expect_err("lenient recovery must not suppress traversal budgets");

    assert!(error.is_resource_error());
    assert_eq!(reader.position(), 0);
}

#[test]
fn lenient_scan_does_not_continue_past_an_unknown_extent() {
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);
    root.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "m_Broken".to_string(),
        -1,
    ));
    root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Marker".to_string(),
        -1,
    ));
    let mut tree = TypeTree::new();
    tree.add_node(root);
    let schema = TypeTreeSchema::compile(&tree, &[], &mut AssetLoadBudget::default()).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&8_i32.to_le_bytes());
    bytes.extend_from_slice(&0x1122_3344_i32.to_le_bytes());
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let error = schema
        .scan_pptrs_with_options(
            &mut reader,
            &mut AssetLoadBudget::default(),
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
        .expect_err("lenient scan cannot infer where the malformed string ends");

    assert!(error.to_string().contains("could not prove"));
    assert_eq!(reader.position(), 4);
}
