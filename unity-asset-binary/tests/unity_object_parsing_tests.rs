//! Tests for UnityObject parsing into specific Unity types

#![allow(unused_imports)]

use indexmap::IndexMap;
use unity_asset_binary::{GameObject, Transform, UnityObject, UnityObjectInfo};
use unity_asset_core::{UnityClass, UnityValue};

fn create_mock_gameobject_data() -> Vec<u8> {
    // Create some mock binary data for a GameObject
    vec![0x01, 0x02, 0x03, 0x04] // Placeholder data
}

fn create_mock_transform_data() -> Vec<u8> {
    // Create some mock binary data for a Transform
    vec![0x05, 0x06, 0x07, 0x08] // Placeholder data
}

#[test]
fn test_unity_object_gameobject_detection() {
    // Create UnityObjectInfo for a GameObject (class_id = 1)
    let mut info = UnityObjectInfo::new(12345, 0, 4, 1);
    info.data = create_mock_gameobject_data();

    // Create a UnityClass with GameObject properties
    let mut unity_class = UnityClass::new(1, "GameObject".to_string(), "12345".to_string());
    unity_class.set(
        "m_Name".to_string(),
        UnityValue::String("TestObject".to_string()),
    );
    unity_class.set("m_Layer".to_string(), UnityValue::Integer(0));
    unity_class.set(
        "m_Tag".to_string(),
        UnityValue::String("Untagged".to_string()),
    );
    unity_class.set("m_IsActive".to_string(), UnityValue::Bool(true));

    let unity_object = UnityObject {
        info,
        class: unity_class,
    };

    // Test detection methods
    assert!(unity_object.is_gameobject());
    assert!(!unity_object.is_transform());
    assert_eq!(unity_object.class_name(), "GameObject");
    assert_eq!(unity_object.class_id(), 1);

    // Test parsing as GameObject
    let game_object = unity_object.as_gameobject().unwrap();
    assert_eq!(game_object.name, "TestObject");
    assert_eq!(game_object.layer, 0);
    assert_eq!(game_object.tag, "Untagged");
    assert!(game_object.active);

    // Test that parsing as Transform fails
    assert!(unity_object.as_transform().is_err());
}

#[test]
fn test_unity_object_transform_detection() {
    // Create UnityObjectInfo for a Transform (class_id = 4)
    let mut info = UnityObjectInfo::new(67890, 0, 4, 4);
    info.data = create_mock_transform_data();

    // Create a UnityClass with Transform properties
    let mut unity_class = UnityClass::new(4, "Transform".to_string(), "67890".to_string());

    // Add position
    let mut position = IndexMap::new();
    position.insert("x".to_string(), UnityValue::Float(1.0));
    position.insert("y".to_string(), UnityValue::Float(2.0));
    position.insert("z".to_string(), UnityValue::Float(3.0));
    unity_class.set("m_LocalPosition".to_string(), UnityValue::Object(position));

    // Add rotation (identity quaternion)
    let mut rotation = IndexMap::new();
    rotation.insert("x".to_string(), UnityValue::Float(0.0));
    rotation.insert("y".to_string(), UnityValue::Float(0.0));
    rotation.insert("z".to_string(), UnityValue::Float(0.0));
    rotation.insert("w".to_string(), UnityValue::Float(1.0));
    unity_class.set("m_LocalRotation".to_string(), UnityValue::Object(rotation));

    // Add scale
    let mut scale = IndexMap::new();
    scale.insert("x".to_string(), UnityValue::Float(1.0));
    scale.insert("y".to_string(), UnityValue::Float(1.0));
    scale.insert("z".to_string(), UnityValue::Float(1.0));
    unity_class.set("m_LocalScale".to_string(), UnityValue::Object(scale));

    let unity_object = UnityObject {
        info,
        class: unity_class,
    };

    // Test detection methods
    assert!(!unity_object.is_gameobject());
    assert!(unity_object.is_transform());
    assert_eq!(unity_object.class_name(), "Transform");
    assert_eq!(unity_object.class_id(), 4);

    // Test parsing as Transform
    let transform = unity_object.as_transform().unwrap();
    assert_eq!(transform.position.x, 1.0);
    assert_eq!(transform.position.y, 2.0);
    assert_eq!(transform.position.z, 3.0);
    assert_eq!(transform.rotation.w, 1.0);
    assert_eq!(transform.scale.x, 1.0);

    // Test that parsing as GameObject fails
    assert!(unity_object.as_gameobject().is_err());
}

#[test]
fn test_unity_object_describe() {
    // Test GameObject description
    let mut info = UnityObjectInfo::new(12345, 0, 4, 1);
    info.data = create_mock_gameobject_data();

    let mut unity_class = UnityClass::new(1, "GameObject".to_string(), "12345".to_string());
    unity_class.set(
        "m_Name".to_string(),
        UnityValue::String("MyGameObject".to_string()),
    );

    let unity_object = UnityObject {
        info,
        class: unity_class,
    };

    let description = unity_object.describe();
    assert!(description.contains("GameObject"));
    assert!(description.contains("MyGameObject"));
    assert!(description.contains("ID:1"));
    assert!(description.contains("PathID:12345"));

    // Test unnamed object
    let mut info2 = UnityObjectInfo::new(67890, 0, 4, 4);
    info2.data = create_mock_transform_data();

    let unity_class2 = UnityClass::new(4, "Transform".to_string(), "67890".to_string());
    let unity_object2 = UnityObject {
        info: info2,
        class: unity_class2,
    };

    let description2 = unity_object2.describe();
    assert!(description2.contains("Transform"));
    assert!(description2.contains("<unnamed>"));
    assert!(description2.contains("ID:4"));
    assert!(description2.contains("PathID:67890"));
}

#[test]
fn test_unity_object_with_complex_gameobject() {
    // Create a more complex GameObject with components
    let mut info = UnityObjectInfo::new(11111, 0, 4, 1);
    info.data = create_mock_gameobject_data();

    let mut unity_class = UnityClass::new(1, "GameObject".to_string(), "11111".to_string());
    unity_class.set(
        "m_Name".to_string(),
        UnityValue::String("ComplexObject".to_string()),
    );
    unity_class.set("m_Layer".to_string(), UnityValue::Integer(5));
    unity_class.set(
        "m_Tag".to_string(),
        UnityValue::String("Player".to_string()),
    );
    unity_class.set("m_IsActive".to_string(), UnityValue::Bool(false));

    // Add components
    let mut component1 = IndexMap::new();
    component1.insert("fileID".to_string(), UnityValue::Integer(0));
    component1.insert("pathID".to_string(), UnityValue::Integer(22222));

    let mut component2 = IndexMap::new();
    component2.insert("fileID".to_string(), UnityValue::Integer(0));
    component2.insert("pathID".to_string(), UnityValue::Integer(33333));

    let components = vec![
        UnityValue::Object(component1),
        UnityValue::Object(component2),
    ];
    unity_class.set("m_Component".to_string(), UnityValue::Array(components));

    let unity_object = UnityObject {
        info,
        class: unity_class,
    };

    // Parse and verify
    let game_object = unity_object.as_gameobject().unwrap();
    assert_eq!(game_object.name, "ComplexObject");
    assert_eq!(game_object.layer, 5);
    assert_eq!(game_object.tag, "Player");
    assert!(!game_object.active); // Should be false
    assert_eq!(game_object.components.len(), 2);
    assert_eq!(game_object.components[0].path_id, 22222);
    assert_eq!(game_object.components[1].path_id, 33333);
}

#[test]
fn test_unity_object_with_complex_transform() {
    // Create a Transform with parent and children
    let mut info = UnityObjectInfo::new(44444, 0, 4, 4);
    info.data = create_mock_transform_data();

    let mut unity_class = UnityClass::new(4, "Transform".to_string(), "44444".to_string());

    // Position
    let mut position = IndexMap::new();
    position.insert("x".to_string(), UnityValue::Float(10.5));
    position.insert("y".to_string(), UnityValue::Float(-5.2));
    position.insert("z".to_string(), UnityValue::Float(0.0));
    unity_class.set("m_LocalPosition".to_string(), UnityValue::Object(position));

    // Rotation (90 degrees around Y axis)
    let mut rotation = IndexMap::new();
    rotation.insert("x".to_string(), UnityValue::Float(0.0));
    rotation.insert("y".to_string(), UnityValue::Float(0.707));
    rotation.insert("z".to_string(), UnityValue::Float(0.0));
    rotation.insert("w".to_string(), UnityValue::Float(0.707));
    unity_class.set("m_LocalRotation".to_string(), UnityValue::Object(rotation));

    // Scale
    let mut scale = IndexMap::new();
    scale.insert("x".to_string(), UnityValue::Float(2.0));
    scale.insert("y".to_string(), UnityValue::Float(1.5));
    scale.insert("z".to_string(), UnityValue::Float(0.5));
    unity_class.set("m_LocalScale".to_string(), UnityValue::Object(scale));

    // Parent
    let mut parent = IndexMap::new();
    parent.insert("fileID".to_string(), UnityValue::Integer(0));
    parent.insert("pathID".to_string(), UnityValue::Integer(55555));
    unity_class.set("m_Father".to_string(), UnityValue::Object(parent));

    // Children
    let mut child1 = IndexMap::new();
    child1.insert("fileID".to_string(), UnityValue::Integer(0));
    child1.insert("pathID".to_string(), UnityValue::Integer(66666));

    let mut child2 = IndexMap::new();
    child2.insert("fileID".to_string(), UnityValue::Integer(0));
    child2.insert("pathID".to_string(), UnityValue::Integer(77777));

    let children = vec![UnityValue::Object(child1), UnityValue::Object(child2)];
    unity_class.set("m_Children".to_string(), UnityValue::Array(children));

    let unity_object = UnityObject {
        info,
        class: unity_class,
    };

    // Parse and verify
    let transform = unity_object.as_transform().unwrap();
    assert_eq!(transform.position.x, 10.5);
    assert_eq!(transform.position.y, -5.2);
    assert_eq!(transform.position.z, 0.0);

    assert_eq!(transform.rotation.y, 0.707);
    assert_eq!(transform.rotation.w, 0.707);

    assert_eq!(transform.scale.x, 2.0);
    assert_eq!(transform.scale.y, 1.5);
    assert_eq!(transform.scale.z, 0.5);

    assert!(transform.parent.is_some());
    assert_eq!(transform.parent.as_ref().unwrap().path_id, 55555);

    assert_eq!(transform.children.len(), 2);
    assert_eq!(transform.children[0].path_id, 66666);
    assert_eq!(transform.children[1].path_id, 77777);
}

#[test]
fn test_wrong_class_id_parsing() {
    // Test that trying to parse wrong class ID fails gracefully
    let mut info = UnityObjectInfo::new(99999, 0, 4, 28); // Texture2D class_id
    info.data = vec![0x01, 0x02, 0x03, 0x04];

    let unity_class = UnityClass::new(28, "Texture2D".to_string(), "99999".to_string());
    let unity_object = UnityObject {
        info,
        class: unity_class,
    };

    // Should not be detected as GameObject or Transform
    assert!(!unity_object.is_gameobject());
    assert!(!unity_object.is_transform());

    // Parsing should fail
    assert!(unity_object.as_gameobject().is_err());
    assert!(unity_object.as_transform().is_err());

    // But basic info should work
    assert_eq!(unity_object.class_name(), "Texture2D");
    assert_eq!(unity_object.class_id(), 28);
    assert_eq!(unity_object.path_id(), 99999);
}
