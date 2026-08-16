use indexmap::IndexMap;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{
    TypeTree, TypeTreeNode, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeSchema,
};
use unity_asset_core::{AssetLoadBudget, UnityValue};

fn node(type_name: &str, name: &str) -> TypeTreeNode {
    TypeTreeNode::with_info(type_name.to_string(), name.to_string(), -1)
}

fn vector_node(name: &str, mut element: TypeTreeNode) -> TypeTreeNode {
    element.name = "data".to_string();

    let mut array = node("Array", "Array");
    array.children.push(node("int", "size"));
    array.children.push(element);

    let mut vector = node("vector", name);
    vector.children.push(array);
    vector
}

fn registry_node(name: &str) -> TypeTreeNode {
    let mut registry = node("ManagedReferencesRegistry", name);
    registry.children.push(node("int", "m_Version"));
    registry
}

fn strict_options() -> TypeTreeParseOptions {
    TypeTreeParseOptions {
        mode: TypeTreeParseMode::Strict,
    }
}

#[test]
fn full_root_alignment_consumes_trailing_padding() {
    let mut root = node("Root", "Root");
    root.meta_flags = 0x4000;
    root.children.push(node("UInt8", "m_Value"));

    let mut tree = TypeTree::new();
    tree.add_node(root);

    let bytes = [0xAB, 0, 0, 0];
    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let output = schema
        .read_object(&mut reader, &mut budget, strict_options())
        .unwrap();

    assert_eq!(
        output.properties.get("m_Value"),
        Some(&UnityValue::Integer(0xAB))
    );
    assert_eq!(reader.position(), 4);
}

#[test]
fn array_size_node_alignment_does_not_align_after_count() {
    let mut root = node("Root", "Root");
    root.children.push(node("UInt8", "m_Prefix"));

    let mut vector = vector_node("m_Data", node("UInt8", "data"));
    vector.children[0].children[0].meta_flags = 0x4000;
    root.children.push(vector);

    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = vec![0xA5];
    bytes.extend_from_slice(&1_i32.to_le_bytes());
    bytes.extend_from_slice(&[0x7F, 0xD1, 0xD2, 0xEE]);

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let output = schema
        .read_object(&mut reader, &mut budget, strict_options())
        .unwrap();

    assert_eq!(
        output.properties.get("m_Prefix"),
        Some(&UnityValue::Integer(0xA5))
    );
    assert_eq!(
        output.properties.get("m_Data"),
        Some(&UnityValue::Bytes(vec![0x7F]))
    );
    assert_eq!(reader.position(), 6);
}

#[test]
fn aligned_u8_array_element_aligns_once_after_bulk_payload() {
    let mut element = node("UInt8", "data");
    element.meta_flags = 0x4000;

    let mut root = node("Root", "Root");
    root.children.push(vector_node("m_Data", element));

    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&3_i32.to_le_bytes());
    bytes.extend_from_slice(&[1, 2, 3, 0]);

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let output = schema
        .read_object(&mut reader, &mut budget, strict_options())
        .unwrap();

    assert_eq!(
        output.properties.get("m_Data"),
        Some(&UnityValue::Bytes(vec![1, 2, 3]))
    );
    assert_eq!(output.stats.scalar_element_ops, 0);
    assert_eq!(reader.position(), 8);

    let expected = UnityValue::Object(output.properties.clone());
    let mut compare_budget = AssetLoadBudget::default();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let (equal, stats) = schema
        .compare_value(&mut reader, &mut compare_budget, schema.root(), &expected)
        .unwrap();
    assert!(equal);
    assert_eq!(stats.scalar_element_ops, 0);
}

#[test]
fn bulk_mismatches_consume_the_extent_and_preserve_work_metrics() {
    let mut root = node("Root", "Root");
    root.children
        .push(vector_node("m_Data", node("SInt32", "data")));

    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&3_i32.to_le_bytes());
    bytes.extend_from_slice(&10_i32.to_le_bytes());
    bytes.extend_from_slice(&20_i32.to_le_bytes());
    bytes.extend_from_slice(&30_i32.to_le_bytes());

    let mut compile_budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut compile_budget).unwrap();
    let mismatches = [
        UnityValue::Array(vec![UnityValue::Integer(10)]),
        UnityValue::String("not-an-array".to_owned()),
    ];

    for mismatch in mismatches {
        let expected = UnityValue::Object(IndexMap::from([("m_Data".to_owned(), mismatch)]));
        let mut budget = AssetLoadBudget::default();
        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let (equal, stats) = schema
            .compare_value(&mut reader, &mut budget, schema.root(), &expected)
            .unwrap();

        assert!(!equal);
        assert_eq!(reader.position(), bytes.len() as u64);
        assert_eq!(stats.bulk_runs, 1);
        assert_eq!(stats.bulk_bytes, 3 * 4);
        assert_eq!(stats.scalar_element_ops, 3);
        assert_eq!(stats.owned_bytes, 0);
        assert_eq!(stats.unity_values_materialized, 0);
    }
}

#[test]
fn repeated_managed_registry_has_zero_extent_after_first_occurrence() {
    let mut root = node("Root", "Root");
    root.children.push(registry_node("m_RegistryA"));
    root.children.push(registry_node("m_RegistryB"));
    root.children.push(node("int", "m_Marker"));

    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&7_i32.to_le_bytes());
    bytes.extend_from_slice(&0x1122_3344_i32.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let output = schema
        .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
        .unwrap();

    let registry = output
        .properties
        .get("m_RegistryA")
        .and_then(UnityValue::as_object)
        .expect("the first registry remains writable");
    assert_eq!(registry.get("m_Version"), Some(&UnityValue::Integer(7)));
    assert!(!output.properties.contains_key("m_RegistryB"));
    assert_eq!(
        output.properties.get("m_Marker"),
        Some(&UnityValue::Integer(0x1122_3344))
    );
    assert!(output.warnings.is_empty());
    assert_eq!(reader.position(), bytes.len() as u64);
}

#[test]
fn managed_registry_state_is_inherited_by_later_nested_records() {
    let mut nested = node("Nested", "m_Nested");
    nested.children.push(registry_node("m_NestedRegistry"));
    nested.children.push(node("int", "m_Marker"));

    let mut root = node("Root", "Root");
    root.children.push(registry_node("m_RootRegistry"));
    root.children.push(nested);
    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&7_i32.to_le_bytes());
    bytes.extend_from_slice(&0x1122_3344_i32.to_le_bytes());
    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);

    let output = schema
        .read_object(&mut reader, &mut budget, strict_options())
        .unwrap();
    let nested = output
        .properties
        .get("m_Nested")
        .and_then(UnityValue::as_object)
        .unwrap();
    assert!(!nested.contains_key("m_NestedRegistry"));
    assert_eq!(
        nested.get("m_Marker"),
        Some(&UnityValue::Integer(0x1122_3344))
    );
    assert_eq!(reader.position(), bytes.len() as u64);
}

#[test]
fn nested_registry_state_does_not_escape_to_parent_siblings() {
    let mut nested = node("Nested", "m_Nested");
    nested.children.push(registry_node("m_NestedRegistry"));
    nested.children.push(node("int", "m_Marker"));

    let mut root = node("Root", "Root");
    root.children.push(nested);
    root.children.push(registry_node("m_RootRegistry"));
    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&7_i32.to_le_bytes());
    bytes.extend_from_slice(&0x1122_3344_i32.to_le_bytes());
    bytes.extend_from_slice(&9_i32.to_le_bytes());
    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);

    let output = schema
        .read_object(&mut reader, &mut budget, strict_options())
        .unwrap();
    let nested = output
        .properties
        .get("m_Nested")
        .and_then(UnityValue::as_object)
        .unwrap();
    assert!(nested.contains_key("m_NestedRegistry"));
    assert_eq!(
        nested.get("m_Marker"),
        Some(&UnityValue::Integer(0x1122_3344))
    );
    assert!(output.properties.contains_key("m_RootRegistry"));
    assert_eq!(reader.position(), bytes.len() as u64);
}

#[test]
fn uint64_scalar_and_array_preserve_values_above_i64_max() {
    let mut root = node("Root", "Root");
    root.children.push(node("UInt64", "m_Scalar"));
    root.children
        .push(vector_node("m_Array", node("UInt64", "data")));

    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    bytes.extend_from_slice(&1_i32.to_le_bytes());
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let output = schema
        .read_object(&mut reader, &mut budget, strict_options())
        .unwrap();

    assert_eq!(
        output.properties.get("m_Scalar"),
        Some(&UnityValue::Unsigned(u64::MAX))
    );
    let values = output
        .properties
        .get("m_Array")
        .and_then(UnityValue::as_array)
        .expect("m_Array should remain an array");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0], UnityValue::Unsigned(u64::MAX));
    assert_eq!(output.stats.scalar_element_ops, 2);
    assert_eq!(reader.position(), bytes.len() as u64);

    let expected = UnityValue::Object(output.properties.clone());
    let mut compare_budget = AssetLoadBudget::default();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let (equal, stats) = schema
        .compare_value(&mut reader, &mut compare_budget, schema.root(), &expected)
        .unwrap();
    assert!(equal);
    assert_eq!(stats.scalar_element_ops, 2);
}

#[test]
fn lenient_unknown_extent_failure_stops_before_sibling() {
    let mut root = node("Root", "Root");
    root.children.push(node("string", "m_Broken"));
    root.children.push(node("int", "m_Marker"));

    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&8_i32.to_le_bytes());
    bytes.extend_from_slice(&0x1122_3344_i32.to_le_bytes());

    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let output = schema
        .read_object(
            &mut reader,
            &mut budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
        .unwrap();

    assert!(!output.properties.contains_key("m_Broken"));
    assert!(!output.properties.contains_key("m_Marker"));
    assert_eq!(output.warnings.len(), 1);
    assert_eq!(output.warnings[0].field, "m_Broken");
    assert_eq!(reader.position(), 4);
}

#[test]
fn lenient_invalid_utf8_uses_proven_string_extent_and_resumes_aligned_sibling() {
    let mut root = node("Root", "Root");
    root.children.push(node("string", "m_Broken"));
    root.children.push(node("int", "m_Marker"));

    let mut tree = TypeTree::new();
    tree.add_node(root);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&2_i32.to_le_bytes());
    bytes.extend_from_slice(&[0xff, 0xfe, 0xa1, 0xb2]);
    bytes.extend_from_slice(&0x1122_3344_i32.to_le_bytes());

    let mut compile_budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut compile_budget).unwrap();
    let mut budget = AssetLoadBudget::default();
    let before = budget.usage();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let output = schema
        .read_object(
            &mut reader,
            &mut budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
        .unwrap();

    assert!(output.complete);
    assert!(!output.properties.contains_key("m_Broken"));
    assert_eq!(
        output.properties.get("m_Marker"),
        Some(&UnityValue::Integer(0x1122_3344))
    );
    assert_eq!(output.warnings.len(), 1);
    assert_eq!(output.warnings[0].field, "m_Broken");
    assert_eq!(reader.position(), bytes.len() as u64);
    assert!(output.stats.wire_bytes >= bytes.len() as u64);
    let after = budget.usage();
    assert!(after.entries >= before.entries);
    assert!(after.bytes >= before.bytes);
    assert!(after.members >= before.members);
    assert!(after.max_observed_depth >= before.max_observed_depth);
}

#[test]
fn duplicate_nonempty_record_names_are_rejected_before_object_materialization() {
    let mut root = node("Root", "Root");
    root.children.push(node("int", "m_Value"));
    root.children.push(node("UInt8", "m_Value"));

    let mut tree = TypeTree::new();
    tree.add_node(root);
    let mut budget = AssetLoadBudget::default();
    let error = TypeTreeSchema::compile(&tree, &[], &mut budget)
        .expect_err("duplicate object keys must not be compiled");

    assert!(error.to_string().contains("duplicate non-empty child name"));
}

#[test]
fn duplicate_registry_name_remains_valid_when_later_registry_has_zero_extent() {
    let mut root = node("Root", "Root");
    root.children.push(registry_node("m_Registry"));
    root.children.push(registry_node("m_Registry"));
    root.children.push(node("int", "m_Marker"));

    let mut tree = TypeTree::new();
    tree.add_node(root);
    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&7_i32.to_le_bytes());
    bytes.extend_from_slice(&0x1122_3344_i32.to_le_bytes());
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let output = schema
        .read_object(&mut reader, &mut budget, strict_options())
        .unwrap();

    assert_eq!(
        output
            .properties
            .get("m_Registry")
            .and_then(UnityValue::as_object)
            .and_then(|registry| registry.get("m_Version")),
        Some(&UnityValue::Integer(7))
    );
    assert_eq!(
        output.properties.get("m_Marker"),
        Some(&UnityValue::Integer(0x1122_3344))
    );
    assert_eq!(reader.position(), bytes.len() as u64);
}

#[test]
fn value_traversal_rejects_a_node_from_another_schema() {
    let mut first_root = node("Root", "Root");
    first_root.children.push(node("UInt8", "m_First"));
    let mut first_tree = TypeTree::new();
    first_tree.add_node(first_root);

    let mut second_root = node("Root", "Root");
    second_root.children.push(node("UInt8", "m_Second"));
    let mut second_tree = TypeTree::new();
    second_tree.add_node(second_root);

    let mut compile_budget = AssetLoadBudget::default();
    let first = TypeTreeSchema::compile(&first_tree, &[], &mut compile_budget).unwrap();
    let second = TypeTreeSchema::compile(&second_tree, &[], &mut compile_budget).unwrap();
    let foreign = second.root().child(0).unwrap();

    let mut budget = AssetLoadBudget::default();
    let mut reader = BinaryReader::new(&[7], ByteOrder::Little);
    assert!(first.read_value(&mut reader, &mut budget, foreign).is_err());

    let mut budget = AssetLoadBudget::default();
    let mut reader = BinaryReader::new(&[7], ByteOrder::Little);
    assert!(first.skip_value(&mut reader, &mut budget, foreign).is_err());
}
