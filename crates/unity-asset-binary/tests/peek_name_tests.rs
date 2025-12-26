use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode, TypeTreeSerializer};

fn le_i32(v: i32) -> [u8; 4] {
    v.to_le_bytes()
}

#[test]
fn typetree_name_peek_prefix_parses_only_until_name() {
    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Id".to_string(),
        4,
    ));
    root.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "m_Name".to_string(),
        -1,
    ));
    root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Flags".to_string(),
        4,
    ));
    tree.add_node(root);

    let (prefix_len, field) = tree
        .name_peek_prefix()
        .expect("expected a name peek prefix");
    assert_eq!(prefix_len, 2);
    assert_eq!(field, "m_Name");

    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&le_i32(7)); // m_Id
    bytes.extend_from_slice(&le_i32(3)); // string length
    bytes.extend_from_slice(b"foo"); // string bytes
    bytes.push(0); // align to 4
    bytes.extend_from_slice(&le_i32(99)); // m_Flags (should remain unread)

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let serializer = TypeTreeSerializer::new(&tree);
    let out = serializer
        .parse_object_prefix_detailed(
            &mut reader,
            unity_asset_binary::typetree::TypeTreeParseOptions::default(),
            prefix_len,
        )
        .unwrap();

    assert_eq!(
        out.properties.get("m_Name").and_then(|v| v.as_str()),
        Some("foo")
    );

    // Ensure we did not parse beyond the aligned string (int + string + padding).
    assert_eq!(reader.position(), 12);
    assert_eq!(reader.remaining(), 4);
}
