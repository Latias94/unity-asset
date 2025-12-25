use unity_asset_binary::asset::SerializedType;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode, TypeTreeSerializer};
use unity_asset_core::UnityValue;

fn push_aligned_string_le(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
    out.extend_from_slice(bytes);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

#[test]
fn referenced_object_data_is_parsed_via_ref_types() {
    // Build a ref type tree: { m_Value: int }
    let mut ref_tree = TypeTree::new();
    let mut ref_root =
        TypeTreeNode::with_info("MyClass".to_string(), "MyClass".to_string(), -1);
    ref_root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Value".to_string(),
        -1,
    ));
    ref_tree.add_node(ref_root);

    let mut ref_type = SerializedType::new(0);
    ref_type.class_name = "MyClass".to_string();
    ref_type.namespace = "MyNS".to_string();
    ref_type.assembly_name = "MyAsm".to_string();
    ref_type.type_tree = ref_tree;

    // Build an object tree containing a ReferencedObject with a `type` object and `data` payload.
    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    let mut ref_obj =
        TypeTreeNode::with_info("ReferencedObject".to_string(), "m_Ref".to_string(), -1);
    let mut type_node = TypeTreeNode::with_info("TypeInfo".to_string(), "type".to_string(), -1);
    type_node.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "class".to_string(),
        -1,
    ));
    type_node.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "ns".to_string(),
        -1,
    ));
    type_node.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "asm".to_string(),
        -1,
    ));
    ref_obj.children.push(type_node);
    ref_obj.children.push(TypeTreeNode::with_info(
        "ReferencedObjectData".to_string(),
        "data".to_string(),
        -1,
    ));

    root.children.push(ref_obj);
    tree.add_node(root);

    let mut bytes = Vec::new();
    push_aligned_string_le(&mut bytes, "MyClass");
    push_aligned_string_le(&mut bytes, "MyNS");
    push_aligned_string_le(&mut bytes, "MyAsm");
    bytes.extend_from_slice(&123i32.to_le_bytes());

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let serializer = TypeTreeSerializer::new(&tree);
    let out = serializer
        .parse_object_detailed_with_ref_types(
            &mut reader,
            unity_asset_binary::typetree::TypeTreeParseOptions::default(),
            std::slice::from_ref(&ref_type),
        )
        .unwrap();

    let UnityValue::Object(m_ref) = out.properties.get("m_Ref").expect("m_Ref present") else {
        panic!("m_Ref should be object");
    };

    let UnityValue::Object(typ) = m_ref.get("type").expect("type present") else {
        panic!("type should be object");
    };
    assert_eq!(typ.get("class").and_then(|v| v.as_str()), Some("MyClass"));
    assert_eq!(typ.get("ns").and_then(|v| v.as_str()), Some("MyNS"));
    assert_eq!(typ.get("asm").and_then(|v| v.as_str()), Some("MyAsm"));

    let UnityValue::Object(data) = m_ref.get("data").expect("data present") else {
        panic!("data should be object");
    };
    assert_eq!(data.get("m_Value").and_then(|v| v.as_i64()), Some(123));
}

