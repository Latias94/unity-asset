use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode, TypeTreeParseOptions, TypeTreeSchema};
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits};

fn make_primitive_array_tree(element_type: &str) -> TypeTree {
    let mut tree = TypeTree::new();
    let mut root = TypeTreeNode::with_info("Root".to_string(), "Root".to_string(), -1);

    let mut vec_node = TypeTreeNode::with_info("vector".to_string(), "m_A".to_string(), -1);
    let mut array_node = TypeTreeNode::with_info("Array".to_string(), "Array".to_string(), -1);

    array_node.children.push(TypeTreeNode::with_info(
        "int".to_string(),
        "size".to_string(),
        -1,
    ));
    array_node.children.push(TypeTreeNode::with_info(
        element_type.to_string(),
        "data".to_string(),
        -1,
    ));

    vec_node.children.push(array_node);
    root.children.push(vec_node);
    tree.add_node(root);
    tree
}

#[test]
fn numeric_array_fastpath_reads_i32_le() {
    let tree = make_primitive_array_tree("SInt32");
    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&3i32.to_le_bytes());
    bytes.extend_from_slice(&(-1i32).to_le_bytes());
    bytes.extend_from_slice(&(0i32).to_le_bytes());
    bytes.extend_from_slice(&(2i32).to_le_bytes());

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let out = schema
        .read_object_prefix(&mut reader, &mut budget, TypeTreeParseOptions::default(), 1)
        .unwrap();

    let arr = out
        .properties
        .get("m_A")
        .and_then(|v| v.as_array())
        .expect("m_A as array");
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0].as_i64(), Some(-1));
    assert_eq!(arr[1].as_i64(), Some(0));
    assert_eq!(arr[2].as_i64(), Some(2));
    assert_eq!(reader.position(), 4 + 3 * 4);
    assert!(out.stats.bulk_runs > 0);
}

#[test]
fn numeric_array_fastpath_reads_u16_be() {
    let tree = make_primitive_array_tree("UInt16");
    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&2i32.to_be_bytes());
    bytes.extend_from_slice(&0x1234u16.to_be_bytes());
    bytes.extend_from_slice(&0xABCDu16.to_be_bytes());

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Big);
    let out = schema
        .read_object_prefix(&mut reader, &mut budget, TypeTreeParseOptions::default(), 1)
        .unwrap();

    let arr = out
        .properties
        .get("m_A")
        .and_then(|v| v.as_array())
        .expect("m_A as array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0].as_i64(), Some(0x1234));
    assert_eq!(arr[1].as_i64(), Some(0xABCD));
    assert_eq!(reader.position(), 4 + 2 * 2);
    assert!(out.stats.bulk_runs > 0);
}

#[test]
fn numeric_array_fastpath_reads_f32_le() {
    let tree = make_primitive_array_tree("float");
    let mut budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut budget).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&2i32.to_le_bytes());
    bytes.extend_from_slice(&(1.0f32).to_bits().to_le_bytes());
    bytes.extend_from_slice(&(-2.5f32).to_bits().to_le_bytes());

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let out = schema
        .read_object_prefix(&mut reader, &mut budget, TypeTreeParseOptions::default(), 1)
        .unwrap();

    let arr = out
        .properties
        .get("m_A")
        .and_then(|v| v.as_array())
        .expect("m_A as array");
    assert_eq!(arr.len(), 2);
    assert!((arr[0].as_f64().unwrap() - 1.0).abs() < 1e-6);
    assert!((arr[1].as_f64().unwrap() - (-2.5)).abs() < 1e-6);
    assert_eq!(reader.position(), 4 + 2 * 4);
    assert!(out.stats.bulk_runs > 0);
}

#[test]
fn numeric_array_fastpath_charges_every_element_node() {
    let tree = make_primitive_array_tree("SInt32");
    let mut compile_budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut compile_budget).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&2_i32.to_le_bytes());
    bytes.extend_from_slice(&10_i32.to_le_bytes());
    bytes.extend_from_slice(&20_i32.to_le_bytes());
    let limits = AssetLoadLimits {
        max_entries: 2,
        ..AssetLoadLimits::default()
    };

    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    assert!(
        schema
            .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
            .unwrap_err()
            .is_resource_error()
    );

    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    assert!(
        schema
            .skip_value(&mut reader, &mut budget, schema.root())
            .unwrap_err()
            .is_resource_error()
    );

    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    assert!(
        schema
            .scan_pptrs(&mut reader, &mut budget)
            .unwrap_err()
            .is_resource_error()
    );
}

#[test]
fn numeric_array_fastpath_observes_element_depth() {
    let tree = make_primitive_array_tree("SInt32");
    let mut compile_budget = AssetLoadBudget::default();
    let schema = TypeTreeSchema::compile(&tree, &[], &mut compile_budget).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1_i32.to_le_bytes());
    bytes.extend_from_slice(&10_i32.to_le_bytes());
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);

    assert!(
        schema
            .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
            .unwrap_err()
            .is_resource_error()
    );
}
