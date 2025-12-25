use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode, TypeTreeSerializer};
use unity_asset_core::UnityValue;

fn le_i32(v: i32) -> [u8; 4] {
    v.to_le_bytes()
}

#[test]
fn typelessdata_reads_length_prefixed_bytes_and_aligns_when_flagged() {
    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    let mut data_node =
        TypeTreeNode::with_info("TypelessData".to_string(), "m_Data".to_string(), -1);
    data_node.meta_flags = 0x4000; // kAlignBytes
    root.children.push(data_node);

    tree.add_node(root);

    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&le_i32(3));
    bytes.extend_from_slice(b"abc");
    bytes.push(0); // padding to 4-byte alignment
    bytes.extend_from_slice(&le_i32(123456)); // should remain unread

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let serializer = TypeTreeSerializer::new(&tree);
    let out = serializer
        .parse_object_prefix_detailed(
            &mut reader,
            unity_asset_binary::typetree::TypeTreeParseOptions::default(),
            1,
        )
        .unwrap();

    let v = out.properties.get("m_Data").expect("m_Data present");
    let b = v.as_bytes().expect("m_Data as bytes");
    assert_eq!(b, b"abc");

    // Ensure we aligned to 4 after reading 4+3 bytes => position should be 8 (padding consumed).
    assert_eq!(reader.position(), 8);
    assert_eq!(reader.remaining(), 4);

    // Ensure no warnings were emitted.
    assert!(out.warnings.is_empty());

    // Ensure the representation is stable (bytes).
    assert!(matches!(v, UnityValue::Bytes(_)));
}
