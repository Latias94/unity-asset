//! Tests for GameObject and Transform parsing

use indexmap::IndexMap;
use unity_asset_binary::{GameObject, ObjectRef, Quaternion, Transform, Vector3};
use unity_asset_core::UnityValue;

#[test]
fn test_gameobject_parsing() {
    // Create mock TypeTree data for a GameObject
    let mut properties = IndexMap::new();
    properties.insert(
        "m_Name".to_string(),
        UnityValue::String("TestGameObject".to_string()),
    );
    properties.insert("m_Layer".to_string(), UnityValue::Integer(5));
    properties.insert(
        "m_Tag".to_string(),
        UnityValue::String("Player".to_string()),
    );
    properties.insert("m_IsActive".to_string(), UnityValue::Bool(true));

    // Create components array
    let mut component1 = IndexMap::new();
    component1.insert("fileID".to_string(), UnityValue::Integer(0));
    component1.insert("pathID".to_string(), UnityValue::Integer(12345));

    let mut component2 = IndexMap::new();
    component2.insert("fileID".to_string(), UnityValue::Integer(0));
    component2.insert("pathID".to_string(), UnityValue::Integer(67890));

    let components = vec![
        UnityValue::Object(component1),
        UnityValue::Object(component2),
    ];
    properties.insert("m_Component".to_string(), UnityValue::Array(components));

    // Parse GameObject
    let game_object = GameObject::from_typetree(&properties).unwrap();

    // Verify parsed data
    assert_eq!(game_object.name, "TestGameObject");
    assert_eq!(game_object.layer, 5);
    assert_eq!(game_object.tag, "Player");
    assert!(game_object.active);
    assert_eq!(game_object.components.len(), 2);
    assert_eq!(game_object.components[0].path_id, 12345);
    assert_eq!(game_object.components[1].path_id, 67890);
}

#[test]
fn test_transform_parsing() {
    // Create mock TypeTree data for a Transform
    let mut properties = IndexMap::new();

    // Position
    let mut position = IndexMap::new();
    position.insert("x".to_string(), UnityValue::Float(1.5));
    position.insert("y".to_string(), UnityValue::Float(2.0));
    position.insert("z".to_string(), UnityValue::Float(-0.5));
    properties.insert("m_LocalPosition".to_string(), UnityValue::Object(position));

    // Rotation
    let mut rotation = IndexMap::new();
    rotation.insert("x".to_string(), UnityValue::Float(0.0));
    rotation.insert("y".to_string(), UnityValue::Float(0.707));
    rotation.insert("z".to_string(), UnityValue::Float(0.0));
    rotation.insert("w".to_string(), UnityValue::Float(0.707));
    properties.insert("m_LocalRotation".to_string(), UnityValue::Object(rotation));

    // Scale
    let mut scale = IndexMap::new();
    scale.insert("x".to_string(), UnityValue::Float(2.0));
    scale.insert("y".to_string(), UnityValue::Float(2.0));
    scale.insert("z".to_string(), UnityValue::Float(2.0));
    properties.insert("m_LocalScale".to_string(), UnityValue::Object(scale));

    // Parent
    let mut parent = IndexMap::new();
    parent.insert("fileID".to_string(), UnityValue::Integer(0));
    parent.insert("pathID".to_string(), UnityValue::Integer(54321));
    properties.insert("m_Father".to_string(), UnityValue::Object(parent));

    // Children
    let mut child1 = IndexMap::new();
    child1.insert("fileID".to_string(), UnityValue::Integer(0));
    child1.insert("pathID".to_string(), UnityValue::Integer(11111));

    let mut child2 = IndexMap::new();
    child2.insert("fileID".to_string(), UnityValue::Integer(0));
    child2.insert("pathID".to_string(), UnityValue::Integer(22222));

    let children = vec![UnityValue::Object(child1), UnityValue::Object(child2)];
    properties.insert("m_Children".to_string(), UnityValue::Array(children));

    // Parse Transform
    let transform = Transform::from_typetree(&properties).unwrap();

    // Verify parsed data
    assert_eq!(transform.position.x, 1.5);
    assert_eq!(transform.position.y, 2.0);
    assert_eq!(transform.position.z, -0.5);

    assert_eq!(transform.rotation.x, 0.0);
    assert_eq!(transform.rotation.y, 0.707);
    assert_eq!(transform.rotation.z, 0.0);
    assert_eq!(transform.rotation.w, 0.707);

    assert_eq!(transform.scale.x, 2.0);
    assert_eq!(transform.scale.y, 2.0);
    assert_eq!(transform.scale.z, 2.0);

    assert!(transform.parent.is_some());
    assert_eq!(transform.parent.as_ref().unwrap().path_id, 54321);

    assert_eq!(transform.children.len(), 2);
    assert_eq!(transform.children[0].path_id, 11111);
    assert_eq!(transform.children[1].path_id, 22222);
}

#[test]
fn test_vector3_and_quaternion() {
    let vec = Vector3::new(1.0, 2.0, 3.0);
    assert_eq!(vec.x, 1.0);
    assert_eq!(vec.y, 2.0);
    assert_eq!(vec.z, 3.0);

    let quat = Quaternion::new(0.0, 0.0, 0.0, 1.0);
    assert_eq!(quat.x, 0.0);
    assert_eq!(quat.y, 0.0);
    assert_eq!(quat.z, 0.0);
    assert_eq!(quat.w, 1.0);

    let identity = Quaternion::identity();
    assert_eq!(identity.w, 1.0);
    assert_eq!(identity.x, 0.0);
}

#[test]
fn test_object_ref() {
    let obj_ref = ObjectRef::new(0, 12345);
    assert_eq!(obj_ref.file_id, 0);
    assert_eq!(obj_ref.path_id, 12345);
    assert!(!obj_ref.is_null());

    let null_ref = ObjectRef::new(0, 0);
    assert!(null_ref.is_null());
}

#[test]
fn test_gameobject_defaults() {
    let game_object = GameObject::new();
    assert_eq!(game_object.name, "");
    assert_eq!(game_object.layer, 0);
    assert_eq!(game_object.tag, "Untagged");
    assert!(game_object.active);
    assert!(game_object.components.is_empty());
}

#[test]
fn test_transform_defaults() {
    let transform = Transform::new();
    assert_eq!(transform.position.x, 0.0);
    assert_eq!(transform.position.y, 0.0);
    assert_eq!(transform.position.z, 0.0);

    assert_eq!(transform.rotation.x, 0.0);
    assert_eq!(transform.rotation.y, 0.0);
    assert_eq!(transform.rotation.z, 0.0);
    assert_eq!(transform.rotation.w, 1.0);

    assert_eq!(transform.scale.x, 1.0);
    assert_eq!(transform.scale.y, 1.0);
    assert_eq!(transform.scale.z, 1.0);

    assert!(transform.parent.is_none());
    assert!(transform.children.is_empty());
}

#[test]
fn test_partial_data_parsing() {
    // Test GameObject with minimal data
    let mut properties = IndexMap::new();
    properties.insert(
        "m_Name".to_string(),
        UnityValue::String("MinimalObject".to_string()),
    );

    let game_object = GameObject::from_typetree(&properties).unwrap();
    assert_eq!(game_object.name, "MinimalObject");
    assert_eq!(game_object.layer, 0); // Default value
    assert_eq!(game_object.tag, "Untagged"); // Default value
    assert!(game_object.active); // Default value
    assert!(game_object.components.is_empty()); // No components

    // Test Transform with minimal data
    let mut properties = IndexMap::new();
    let mut position = IndexMap::new();
    position.insert("x".to_string(), UnityValue::Float(5.0));
    position.insert("y".to_string(), UnityValue::Float(0.0));
    position.insert("z".to_string(), UnityValue::Float(0.0));
    properties.insert("m_LocalPosition".to_string(), UnityValue::Object(position));

    let transform = Transform::from_typetree(&properties).unwrap();
    assert_eq!(transform.position.x, 5.0);
    assert_eq!(transform.rotation.w, 1.0); // Default identity
    assert_eq!(transform.scale.x, 1.0); // Default scale
}

#[test]
fn test_empty_data_parsing() {
    // Test with completely empty data
    let properties = IndexMap::new();

    let game_object = GameObject::from_typetree(&properties).unwrap();
    assert_eq!(game_object.name, "");
    assert_eq!(game_object.layer, 0);
    assert_eq!(game_object.tag, "Untagged");
    assert!(game_object.active);

    let transform = Transform::from_typetree(&properties).unwrap();
    assert_eq!(transform.position.x, 0.0);
    assert_eq!(transform.rotation.w, 1.0);
    assert_eq!(transform.scale.x, 1.0);
}
