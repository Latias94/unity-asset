use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode, TypeTreeSerializer};

fn make_pptr_node(name: &str) -> TypeTreeNode {
    let mut pptr = TypeTreeNode::with_info("PPtr<Object>".to_string(), name.to_string(), -1);
    pptr.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_FileID".to_string(),
        -1,
    ));
    pptr.children.push(TypeTreeNode::with_info(
        "long long".to_string(),
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

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let serializer = TypeTreeSerializer::new(&tree);
    let scan = serializer.scan_pptrs(&mut reader).unwrap();

    assert_eq!(reader.position() as usize, bytes.len());

    let mut internal = scan.internal.clone();
    internal.sort_unstable();
    internal.dedup();
    assert_eq!(internal, vec![123, 222]);

    let mut external = scan.external.clone();
    external.sort_unstable();
    external.dedup();
    assert_eq!(external, vec![(1, 111)]);
}
