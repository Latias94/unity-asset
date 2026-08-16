use unity_asset_binary::asset::SerializedType;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode, TypeTreeParseOptions, TypeTreeSchema};
use unity_asset_core::{AssetLoadBudget, UnityValue};

fn push_aligned_string_le(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
    out.extend_from_slice(bytes);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

#[test]
fn referenced_object_data_is_parsed_via_ref_types() {
    // Build a ref type tree: { m_Value: int }
    let mut ref_tree = TypeTree::new();
    let mut ref_root = TypeTreeNode::with_info("MyClass".to_string(), "MyClass".to_string(), -1);
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

    let mut budget = AssetLoadBudget::default();
    let schema =
        TypeTreeSchema::compile(&tree, std::slice::from_ref(&ref_type), &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let out = schema
        .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
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

#[test]
fn referenced_object_data_resolves_via_unity_field_aliases() {
    // Unity sometimes encodes managed reference type triplets using m_ClassName/m_NameSpace/m_AssemblyName.
    let mut ref_tree = TypeTree::new();
    let mut ref_root = TypeTreeNode::with_info("MyClass".to_string(), "MyClass".to_string(), -1);
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

    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    let mut ref_obj =
        TypeTreeNode::with_info("ReferencedObject".to_string(), "m_Ref".to_string(), -1);
    let mut type_node = TypeTreeNode::with_info("TypeInfo".to_string(), "type".to_string(), -1);
    type_node.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "m_ClassName".to_string(),
        -1,
    ));
    type_node.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "m_NameSpace".to_string(),
        -1,
    ));
    type_node.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "m_AssemblyName".to_string(),
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
    bytes.extend_from_slice(&456i32.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema =
        TypeTreeSchema::compile(&tree, std::slice::from_ref(&ref_type), &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let out = schema
        .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
        .unwrap();

    let UnityValue::Object(m_ref) = out.properties.get("m_Ref").expect("m_Ref present") else {
        panic!("m_Ref should be object");
    };
    let UnityValue::Object(data) = m_ref.get("data").expect("data present") else {
        panic!("data should be object");
    };
    assert_eq!(data.get("m_Value").and_then(|v| v.as_i64()), Some(456));
    assert_eq!(reader.position() as usize, bytes.len());
}

#[test]
fn referenced_object_fallback_reads_statically_provable_payload() {
    // No ref_types are provided, but this placeholder has a statically provable wire extent.
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
    // Placeholder payload: an int field so we can ensure the reader stays in sync.
    let mut data_node =
        TypeTreeNode::with_info("ReferencedObjectData".to_string(), "data".to_string(), -1);
    data_node.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Value".to_string(),
        -1,
    ));
    ref_obj.children.push(data_node);

    root.children.push(ref_obj);
    tree.add_node(root);

    let mut bytes = Vec::new();
    push_aligned_string_le(&mut bytes, "MissingClass");
    push_aligned_string_le(&mut bytes, "NS");
    push_aligned_string_le(&mut bytes, "ASM");
    bytes.extend_from_slice(&7i32.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let out = schema
        .read_object_prefix(&mut reader, &mut budget, TypeTreeParseOptions::default(), 1)
        .unwrap();

    let UnityValue::Object(m_ref) = out.properties.get("m_Ref").expect("m_Ref present") else {
        panic!("m_Ref should be object");
    };
    let UnityValue::Object(typ) = m_ref.get("type").expect("type present") else {
        panic!("type should be object");
    };
    assert_eq!(
        typ.get("class").and_then(UnityValue::as_str),
        Some("MissingClass")
    );
    assert_eq!(typ.get("ns").and_then(UnityValue::as_str), Some("NS"));
    assert_eq!(typ.get("asm").and_then(UnityValue::as_str), Some("ASM"));

    let UnityValue::Object(data) = m_ref.get("data").expect("data present") else {
        panic!("data should be object");
    };
    assert_eq!(data.get("m_Value").and_then(|v| v.as_i64()), Some(7));
    assert_eq!(reader.position() as usize, bytes.len());
}

#[test]
fn managed_references_registry_is_materialized_without_affecting_following_fields() {
    // The first registry is retained as writable data while traversal stays in sync.
    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    let mut registry = TypeTreeNode::with_info(
        "ManagedReferencesRegistry".to_string(),
        "m_Registry".to_string(),
        -1,
    );
    let mut vec_node = TypeTreeNode::with_info("vector".to_string(), "m_Data".to_string(), -1);
    let mut array_node = TypeTreeNode::with_info("Array".to_string(), "Array".to_string(), -1);
    array_node.meta_flags = 0x4000; // align to 4 after the array payload
    array_node.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "size".to_string(),
        -1,
    ));
    array_node.children.push(TypeTreeNode::with_info(
        "UInt8".to_string(),
        "data".to_string(),
        -1,
    ));
    vec_node.children.push(array_node);
    registry.children.push(vec_node);

    root.children.push(registry);
    root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Next".to_string(),
        -1,
    ));
    tree.add_node(root);

    // Registry bytes:
    // - array size=1
    // - one byte
    // - 3 bytes padding (Array node has align flag)
    // Then m_Next (int).
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1i32.to_le_bytes());
    bytes.push(0xAA);
    bytes.extend_from_slice(&[0u8; 3]);
    bytes.extend_from_slice(&0x11223344i32.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let out = schema
        .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
        .unwrap();

    let registry = out
        .properties
        .get("m_Registry")
        .and_then(UnityValue::as_object)
        .expect("the first registry should remain writable");
    assert_eq!(
        registry.get("m_Data").and_then(UnityValue::as_bytes),
        Some([0xAA].as_slice())
    );
    assert_eq!(
        out.properties.get("m_Next").and_then(|v| v.as_i64()),
        Some(0x11223344)
    );
    assert_eq!(reader.position() as usize, bytes.len());
}

#[test]
fn managed_references_registry_budgetedly_materializes_large_byte_arrays() {
    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    let mut registry = TypeTreeNode::with_info(
        "ManagedReferencesRegistry".to_string(),
        "m_Registry".to_string(),
        -1,
    );
    let mut vec_node = TypeTreeNode::with_info("vector".to_string(), "m_Data".to_string(), -1);
    let mut array_node = TypeTreeNode::with_info("Array".to_string(), "Array".to_string(), -1);
    array_node.meta_flags = 0x4000; // align to 4 after the array payload
    array_node.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "size".to_string(),
        -1,
    ));
    array_node.children.push(TypeTreeNode::with_info(
        "UInt8".to_string(),
        "data".to_string(),
        -1,
    ));
    vec_node.children.push(array_node);
    registry.children.push(vec_node);

    root.children.push(registry);
    root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Next".to_string(),
        -1,
    ));
    tree.add_node(root);

    let n: i32 = 128 * 1024;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&n.to_le_bytes());
    bytes.extend(std::iter::repeat_n(0xABu8, n as usize));
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes.extend_from_slice(&0x55667788i32.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let out = schema
        .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
        .unwrap();

    let data = out
        .properties
        .get("m_Registry")
        .and_then(UnityValue::as_object)
        .and_then(|registry| registry.get("m_Data"))
        .and_then(UnityValue::as_bytes)
        .expect("registry byte array should be materialized");
    assert_eq!(data.len(), n as usize);
    assert_eq!(data.first(), Some(&0xAB));
    assert_eq!(data.last(), Some(&0xAB));
    assert_eq!(
        out.properties.get("m_Next").and_then(|v| v.as_i64()),
        Some(0x55667788)
    );
    assert_eq!(reader.position() as usize, bytes.len());
}

#[test]
fn managed_references_registry_materializes_nested_string_vectors() {
    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    let mut registry = TypeTreeNode::with_info(
        "ManagedReferencesRegistry".to_string(),
        "m_Registry".to_string(),
        -1,
    );
    registry.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Version".to_string(),
        -1,
    ));

    let mut vec_node = TypeTreeNode::with_info("vector".to_string(), "m_Names".to_string(), -1);
    let mut array_node = TypeTreeNode::with_info("Array".to_string(), "Array".to_string(), -1);
    array_node.meta_flags = 0x4000; // align to 4 after the array payload
    array_node.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "size".to_string(),
        -1,
    ));
    array_node.children.push(TypeTreeNode::with_info(
        "string".to_string(),
        "data".to_string(),
        -1,
    ));
    vec_node.children.push(array_node);
    registry.children.push(vec_node);

    root.children.push(registry);
    root.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "m_Next".to_string(),
        -1,
    ));
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&2i32.to_le_bytes()); // m_Version
    bytes.extend_from_slice(&2i32.to_le_bytes()); // size
    push_aligned_string_le(&mut bytes, "a");
    push_aligned_string_le(&mut bytes, "bc");
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes.extend_from_slice(&0x01020304i32.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let out = schema
        .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
        .unwrap();

    let registry = out
        .properties
        .get("m_Registry")
        .and_then(UnityValue::as_object)
        .expect("registry should be materialized");
    assert_eq!(
        registry.get("m_Version").and_then(UnityValue::as_i64),
        Some(2)
    );
    let names = registry
        .get("m_Names")
        .and_then(UnityValue::as_array)
        .expect("registry names should be an array");
    assert_eq!(
        names.iter().map(UnityValue::as_str).collect::<Vec<_>>(),
        vec![Some("a"), Some("bc")]
    );
    assert_eq!(
        out.properties.get("m_Next").and_then(|v| v.as_i64()),
        Some(0x01020304)
    );
    assert_eq!(reader.position() as usize, bytes.len());
}

#[test]
fn scan_pptrs_can_traverse_managed_reference_payloads_via_ref_types() {
    // Build a ref type tree: { m_Ptr: PPtr<Object> }.
    let mut ref_tree = TypeTree::new();
    let mut ref_root = TypeTreeNode::with_info("MyClass".to_string(), "MyClass".to_string(), -1);
    let mut pptr = TypeTreeNode::with_info("PPtr<Object>".to_string(), "m_Ptr".to_string(), -1);
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
    ref_root.children.push(pptr);
    ref_tree.add_node(ref_root);

    let mut ref_type = SerializedType::new(0);
    ref_type.class_name = "MyClass".to_string();
    ref_type.namespace = "MyNS".to_string();
    ref_type.assembly_name = "MyAsm".to_string();
    ref_type.type_tree = ref_tree;

    // Root contains ReferencedObject -> type triplet + data payload.
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
    bytes.extend_from_slice(&0i32.to_le_bytes()); // m_FileID
    bytes.extend_from_slice(&1234i64.to_le_bytes()); // m_PathID

    let mut budget = AssetLoadBudget::default();
    let schema =
        TypeTreeSchema::compile(&tree, std::slice::from_ref(&ref_type), &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let scan = schema.scan_pptrs(&mut reader, &mut budget).unwrap();

    assert_eq!(scan.internal, vec![1234]);
    assert!(scan.external.is_empty());
    assert_eq!(scan.stats.unity_values_materialized, 0);
    assert_eq!(reader.position() as usize, bytes.len());
}
