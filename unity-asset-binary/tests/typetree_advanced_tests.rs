//! Advanced TypeTree parsing tests

use indexmap::IndexMap;
use unity_asset_binary::{BinaryReader, ByteOrder, TypeTree, TypeTreeNode};
use unity_asset_core::UnityValue;

/// Create a mock TypeTree for testing
fn create_mock_typetree() -> TypeTree {
    let mut tree = TypeTree::new();
    tree.version = 10;

    // Create a simple GameObject-like structure
    let mut root = TypeTreeNode::new();
    root.type_name = "GameObject".to_string();
    root.name = "".to_string(); // Root has no name
    root.level = 0;
    root.byte_size = -1; // Variable size

    // m_Name field
    let mut name_node = TypeTreeNode::new();
    name_node.type_name = "string".to_string();
    name_node.name = "m_Name".to_string();
    name_node.level = 1;
    name_node.byte_size = -1;

    // m_IsActive field
    let mut active_node = TypeTreeNode::new();
    active_node.type_name = "bool".to_string();
    active_node.name = "m_IsActive".to_string();
    active_node.level = 1;
    active_node.byte_size = 1;

    // m_Layer field
    let mut layer_node = TypeTreeNode::new();
    layer_node.type_name = "int".to_string();
    layer_node.name = "m_Layer".to_string();
    layer_node.level = 1;
    layer_node.byte_size = 4;

    root.children = vec![name_node, active_node, layer_node];
    tree.nodes = vec![root];

    tree
}

/// Create mock binary data for a GameObject
fn create_mock_gameobject_data() -> Vec<u8> {
    let mut data = Vec::new();

    // String: "TestObject" (Unity string format: length + data)
    let name = b"TestObject";
    data.extend_from_slice(&(name.len() as u32).to_le_bytes()); // Length
    data.extend_from_slice(name); // String data
                                  // Align to 4 bytes after string
    while data.len() % 4 != 0 {
        data.push(0);
    }

    // Bool: true (1 byte)
    data.push(1);
    // Align to 4 bytes
    while data.len() % 4 != 0 {
        data.push(0);
    }

    // Int: layer 5 (4 bytes)
    data.extend_from_slice(&5i32.to_le_bytes());

    data
}

#[test]
fn test_typetree_dict_parsing() {
    let tree = create_mock_typetree();
    let data = create_mock_gameobject_data();

    println!("Test data: {:?}", data);
    println!("Test data length: {}", data.len());

    let mut reader = BinaryReader::new(&data, ByteOrder::Little);

    let result = tree.parse_as_dict(&mut reader);
    assert!(
        result.is_ok(),
        "TypeTree parsing should succeed: {:?}",
        result.err()
    );

    let properties = result.unwrap();

    println!("Parsed properties: {:?}", properties);

    // Check that we have the expected properties
    assert!(
        properties.contains_key("m_Name"),
        "Should have m_Name property"
    );
    assert!(
        properties.contains_key("m_IsActive"),
        "Should have m_IsActive property"
    );
    assert!(
        properties.contains_key("m_Layer"),
        "Should have m_Layer property"
    );

    // Check property values
    if let Some(UnityValue::String(name)) = properties.get("m_Name") {
        assert_eq!(name, "TestObject");
    } else {
        panic!(
            "m_Name should be a string with value 'TestObject', got: {:?}",
            properties.get("m_Name")
        );
    }

    if let Some(UnityValue::Bool(active)) = properties.get("m_IsActive") {
        assert!(*active, "m_IsActive should be true");
    } else {
        panic!(
            "m_IsActive should be a boolean with value true, got: {:?}",
            properties.get("m_IsActive")
        );
    }

    if let Some(UnityValue::Integer(layer)) = properties.get("m_Layer") {
        assert_eq!(*layer, 5);
    } else {
        panic!(
            "m_Layer should be an integer with value 5, got: {:?}",
            properties.get("m_Layer")
        );
    }
}

#[test]
fn test_typetree_primitive_types() {
    let mut tree = TypeTree::new();
    tree.version = 10;

    // Create nodes for different primitive types
    let mut root = TypeTreeNode::new();
    root.type_name = "TestClass".to_string();
    root.name = "".to_string();
    root.level = 0;

    // Test different integer types
    let mut int8_node = TypeTreeNode::new();
    int8_node.type_name = "SInt8".to_string();
    int8_node.name = "m_Int8".to_string();
    int8_node.level = 1;
    int8_node.byte_size = 1;

    let mut uint32_node = TypeTreeNode::new();
    uint32_node.type_name = "UInt32".to_string();
    uint32_node.name = "m_UInt32".to_string();
    uint32_node.level = 1;
    uint32_node.byte_size = 4;

    let mut float_node = TypeTreeNode::new();
    float_node.type_name = "float".to_string();
    float_node.name = "m_Float".to_string();
    float_node.level = 1;
    float_node.byte_size = 4;

    root.children = vec![int8_node, uint32_node, float_node];
    tree.nodes = vec![root];

    // Create test data
    let mut data = Vec::new();
    data.push(-42i8 as u8); // SInt8: -42
                            // Align to 4 bytes for next field
    while data.len() % 4 != 0 {
        data.push(0);
    }
    data.extend_from_slice(&12345u32.to_le_bytes()); // UInt32: 12345
    data.extend_from_slice(&3.14159f32.to_le_bytes()); // float: 3.14159

    let mut reader = BinaryReader::new(&data, ByteOrder::Little);
    let result = tree.parse_as_dict(&mut reader).unwrap();

    // Verify parsed values
    if let Some(UnityValue::Integer(val)) = result.get("m_Int8") {
        assert_eq!(*val, -42);
    } else {
        panic!("m_Int8 should be -42");
    }

    if let Some(UnityValue::Integer(val)) = result.get("m_UInt32") {
        assert_eq!(*val, 12345);
    } else {
        panic!("m_UInt32 should be 12345");
    }

    if let Some(UnityValue::Float(val)) = result.get("m_Float") {
        assert!((val - 3.14159).abs() < 0.0001);
    } else {
        panic!("m_Float should be approximately 3.14159");
    }
}

#[test]
fn test_typetree_nested_objects() {
    let mut tree = TypeTree::new();
    tree.version = 10;

    // Create a nested structure: Transform with Vector3 position
    let mut root = TypeTreeNode::new();
    root.type_name = "Transform".to_string();
    root.name = "".to_string();
    root.level = 0;

    // m_LocalPosition (Vector3)
    let mut position_node = TypeTreeNode::new();
    position_node.type_name = "Vector3f".to_string();
    position_node.name = "m_LocalPosition".to_string();
    position_node.level = 1;

    // Vector3 components
    let mut x_node = TypeTreeNode::new();
    x_node.type_name = "float".to_string();
    x_node.name = "x".to_string();
    x_node.level = 2;
    x_node.byte_size = 4;

    let mut y_node = TypeTreeNode::new();
    y_node.type_name = "float".to_string();
    y_node.name = "y".to_string();
    y_node.level = 2;
    y_node.byte_size = 4;

    let mut z_node = TypeTreeNode::new();
    z_node.type_name = "float".to_string();
    z_node.name = "z".to_string();
    z_node.level = 2;
    z_node.byte_size = 4;

    position_node.children = vec![x_node, y_node, z_node];
    root.children = vec![position_node];
    tree.nodes = vec![root];

    // Create test data: Vector3(1.5, 2.0, -0.5)
    let mut data = Vec::new();
    data.extend_from_slice(&1.5f32.to_le_bytes()); // x
    data.extend_from_slice(&2.0f32.to_le_bytes()); // y
    data.extend_from_slice(&(-0.5f32).to_le_bytes()); // z

    let mut reader = BinaryReader::new(&data, ByteOrder::Little);
    let result = tree.parse_as_dict(&mut reader).unwrap();

    // Verify nested structure
    assert!(result.contains_key("m_LocalPosition"));

    if let Some(UnityValue::Object(position)) = result.get("m_LocalPosition") {
        if let Some(UnityValue::Float(x)) = position.get("x") {
            assert!((x - 1.5).abs() < 0.0001);
        } else {
            panic!("x should be 1.5");
        }

        if let Some(UnityValue::Float(y)) = position.get("y") {
            assert!((y - 2.0).abs() < 0.0001);
        } else {
            panic!("y should be 2.0");
        }

        if let Some(UnityValue::Float(z)) = position.get("z") {
            assert!((z - (-0.5)).abs() < 0.0001);
        } else {
            panic!("z should be -0.5");
        }
    } else {
        panic!("m_LocalPosition should be an object");
    }
}

#[test]
fn test_typetree_alignment() {
    let mut tree = TypeTree::new();
    tree.version = 10;

    // Create a node that requires alignment
    let mut root = TypeTreeNode::new();
    root.type_name = "TestClass".to_string();
    root.name = "".to_string();
    root.level = 0;

    let mut aligned_node = TypeTreeNode::new();
    aligned_node.type_name = "int".to_string();
    aligned_node.name = "m_AlignedInt".to_string();
    aligned_node.level = 1;
    aligned_node.byte_size = 4;
    aligned_node.meta_flags = 0x4000; // ALIGN_BYTES flag

    root.children = vec![aligned_node];
    tree.nodes = vec![root];

    // Test alignment checking
    assert!(tree.is_aligned(&tree.nodes[0].children[0]));

    // Create test data with padding
    let mut data = Vec::new();
    data.push(0xFF); // Some padding byte
    data.extend_from_slice(&[0, 0, 0]); // Align to 4 bytes
    data.extend_from_slice(&42i32.to_le_bytes()); // Aligned integer

    let mut reader = BinaryReader::new(&data, ByteOrder::Little);

    // Skip the padding byte manually (in real scenarios, alignment would handle this)
    reader.read_u8().unwrap();

    let result = tree.parse_as_dict(&mut reader).unwrap();

    if let Some(UnityValue::Integer(val)) = result.get("m_AlignedInt") {
        assert_eq!(*val, 42);
    } else {
        panic!("m_AlignedInt should be 42");
    }
}

#[test]
fn test_empty_typetree() {
    let tree = TypeTree::new();
    let data = vec![0u8; 16]; // Some dummy data
    let mut reader = BinaryReader::new(&data, ByteOrder::Little);

    let result = tree.parse_as_dict(&mut reader).unwrap();
    assert!(
        result.is_empty(),
        "Empty TypeTree should produce empty result"
    );
}

#[test]
fn test_typetree_error_handling() {
    let mut tree = TypeTree::new();
    tree.version = 10;

    // Create a node that expects more data than available
    let mut root = TypeTreeNode::new();
    root.type_name = "TestClass".to_string();
    root.name = "".to_string();
    root.level = 0;

    let mut int_node = TypeTreeNode::new();
    int_node.type_name = "int".to_string();
    int_node.name = "m_Int".to_string();
    int_node.level = 1;
    int_node.byte_size = 4;

    root.children = vec![int_node];
    tree.nodes = vec![root];

    // Provide insufficient data (only 2 bytes instead of 4)
    let data = vec![0x01, 0x02];
    let mut reader = BinaryReader::new(&data, ByteOrder::Little);

    let result = tree.parse_as_dict(&mut reader);
    assert!(
        result.is_err(),
        "Should fail when insufficient data is available"
    );
}
