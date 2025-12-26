//! Tests for Unity YAML serialization functionality
//!
//! These tests verify that our serialization produces valid Unity YAML
//! that can be round-tripped successfully.

use std::collections::HashMap;
use unity_asset_core::{UnityClass, UnityDocument, UnityValue};
use unity_asset_yaml::{SerdeUnityLoader, UnityYamlSerializer, YamlDocument};

/// Test basic serialization of a simple GameObject
#[test]
fn test_serialize_simple_gameobject() {
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123456789".to_string());

    gameobject.set("m_ObjectHideFlags".to_string(), UnityValue::Integer(0));
    gameobject.set(
        "m_Name".to_string(),
        UnityValue::String("TestObject".to_string()),
    );
    gameobject.set(
        "m_TagString".to_string(),
        UnityValue::String("Untagged".to_string()),
    );
    gameobject.set("m_Layer".to_string(), UnityValue::Integer(0));
    gameobject.set("m_IsActive".to_string(), UnityValue::Bool(true));

    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serializer.serialize_to_string(&[gameobject]).unwrap();

    // Verify YAML structure
    assert!(yaml_output.contains("%YAML 1.1"));
    assert!(yaml_output.contains("%TAG !u! tag:unity3d.com,2011:"));
    assert!(yaml_output.contains("--- !u!1 &123456789"));
    assert!(yaml_output.contains("GameObject:"));
    assert!(yaml_output.contains("m_Name: TestObject"));
    assert!(yaml_output.contains("m_IsActive: 1"));

    println!("Generated YAML:\n{}", yaml_output);
}

/// Test serialization of complex nested objects
#[test]
fn test_serialize_complex_transform() {
    let mut transform = UnityClass::new(4, "Transform".to_string(), "987654321".to_string());

    // Add nested position object
    let mut position = HashMap::new();
    position.insert("x".to_string(), UnityValue::Float(1.5));
    position.insert("y".to_string(), UnityValue::Float(2.0));
    position.insert("z".to_string(), UnityValue::Float(-0.5));
    transform.set(
        "m_LocalPosition".to_string(),
        UnityValue::Object(position.into_iter().collect()),
    );

    // Add nested rotation object
    let mut rotation = HashMap::new();
    rotation.insert("x".to_string(), UnityValue::Float(0.0));
    rotation.insert("y".to_string(), UnityValue::Float(0.0));
    rotation.insert("z".to_string(), UnityValue::Float(0.0));
    rotation.insert("w".to_string(), UnityValue::Float(1.0));
    transform.set(
        "m_LocalRotation".to_string(),
        UnityValue::Object(rotation.into_iter().collect()),
    );

    // Add array of children
    let children = vec![
        UnityValue::Integer(111111),
        UnityValue::Integer(222222),
        UnityValue::Integer(333333),
    ];
    transform.set("m_Children".to_string(), UnityValue::Array(children));

    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serializer.serialize_to_string(&[transform]).unwrap();

    // Verify complex structure
    assert!(yaml_output.contains("--- !u!4 &987654321"));
    assert!(yaml_output.contains("Transform:"));
    assert!(yaml_output.contains("m_LocalPosition:"));
    assert!(yaml_output.contains("m_LocalRotation:"));
    assert!(yaml_output.contains("m_Children:"));

    println!("Generated complex YAML:\n{}", yaml_output);
}

/// Test round-trip serialization (serialize -> parse -> serialize)
#[test]
fn test_round_trip_serialization() {
    // Create original data
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123456789".to_string());
    gameobject.set(
        "m_Name".to_string(),
        UnityValue::String("RoundTripTest".to_string()),
    );
    gameobject.set("m_IsActive".to_string(), UnityValue::Bool(true));

    let mut position = HashMap::new();
    position.insert("x".to_string(), UnityValue::Float(1.0));
    position.insert("y".to_string(), UnityValue::Float(2.0));
    position.insert("z".to_string(), UnityValue::Float(3.0));
    gameobject.set(
        "m_Position".to_string(),
        UnityValue::Object(position.into_iter().collect()),
    );

    let original_classes = vec![gameobject];

    // First serialization
    let mut serializer = UnityYamlSerializer::new();
    let yaml1 = serializer.serialize_to_string(&original_classes).unwrap();

    // Parse back
    let loader = SerdeUnityLoader::new();
    let parsed_classes = loader.load_from_str(&yaml1).unwrap();

    // Second serialization
    let yaml2 = serializer.serialize_to_string(&parsed_classes).unwrap();

    // Verify data integrity
    assert_eq!(original_classes.len(), parsed_classes.len());

    let original = &original_classes[0];
    let parsed = &parsed_classes[0];

    assert_eq!(original.class_name, parsed.class_name);
    assert_eq!(original.class_id, parsed.class_id);
    assert_eq!(original.anchor, parsed.anchor);

    // Check specific properties
    assert_eq!(original.get("m_Name"), parsed.get("m_Name"));

    // Note: Unity YAML represents booleans as integers (1/0)
    // So Bool(true) becomes Integer(1) after round-trip
    match (original.get("m_IsActive"), parsed.get("m_IsActive")) {
        (Some(UnityValue::Bool(true)), Some(UnityValue::Integer(1))) => {
            // This is expected - Unity represents true as 1
        }
        (Some(UnityValue::Bool(false)), Some(UnityValue::Integer(0))) => {
            // This is expected - Unity represents false as 0
        }
        (orig, parsed) => {
            panic!("Unexpected boolean conversion: {:?} -> {:?}", orig, parsed);
        }
    }

    println!("First YAML:\n{}", yaml1);
    println!("Second YAML:\n{}", yaml2);
}

/// Test serialization of multiple documents
#[test]
fn test_serialize_multiple_documents() {
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
    gameobject.set(
        "m_Name".to_string(),
        UnityValue::String("Object1".to_string()),
    );

    let mut transform = UnityClass::new(4, "Transform".to_string(), "456".to_string());
    let mut pos = HashMap::new();
    pos.insert("x".to_string(), UnityValue::Float(0.0));
    pos.insert("y".to_string(), UnityValue::Float(0.0));
    pos.insert("z".to_string(), UnityValue::Float(0.0));
    transform.set(
        "m_LocalPosition".to_string(),
        UnityValue::Object(pos.into_iter().collect()),
    );

    let mut monobehaviour = UnityClass::new(114, "MonoBehaviour".to_string(), "789".to_string());
    monobehaviour.set("m_Enabled".to_string(), UnityValue::Bool(true));

    let classes = vec![gameobject, transform, monobehaviour];

    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serializer.serialize_to_string(&classes).unwrap();

    // Should have YAML header only once
    let yaml_header_count = yaml_output.matches("%YAML 1.1").count();
    assert_eq!(yaml_header_count, 1);

    // Should have three document separators
    let doc_separator_count = yaml_output.matches("--- !u!").count();
    assert_eq!(doc_separator_count, 3);

    // Should contain all three class types
    assert!(yaml_output.contains("GameObject:"));
    assert!(yaml_output.contains("Transform:"));
    assert!(yaml_output.contains("MonoBehaviour:"));

    println!("Multi-document YAML:\n{}", yaml_output);
}

/// Test YamlDocument save and load functionality
#[test]
fn test_yaml_document_serialization() {
    // Create a YamlDocument
    let mut doc = YamlDocument::new();

    // Add Unity classes
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
    gameobject.set(
        "m_Name".to_string(),
        UnityValue::String("DocumentTest".to_string()),
    );
    gameobject.set("m_IsActive".to_string(), UnityValue::Bool(true));
    doc.add_entry(gameobject);

    let mut transform = UnityClass::new(4, "Transform".to_string(), "456".to_string());
    let mut pos = HashMap::new();
    pos.insert("x".to_string(), UnityValue::Float(1.0));
    pos.insert("y".to_string(), UnityValue::Float(2.0));
    pos.insert("z".to_string(), UnityValue::Float(3.0));
    transform.set(
        "m_LocalPosition".to_string(),
        UnityValue::Object(pos.into_iter().collect()),
    );
    doc.add_entry(transform);

    // Test dump_yaml
    let yaml_content = doc.dump_yaml().unwrap();

    // Verify structure
    assert!(yaml_content.contains("%YAML 1.1"));
    assert!(yaml_content.contains("GameObject:"));
    assert!(yaml_content.contains("Transform:"));
    assert!(yaml_content.contains("m_Name: DocumentTest"));
    assert!(yaml_content.contains("m_LocalPosition:"));

    // Test round-trip through string
    let loader = SerdeUnityLoader::new();
    let parsed_classes = loader.load_from_str(&yaml_content).unwrap();

    assert_eq!(parsed_classes.len(), 2);
    assert_eq!(parsed_classes[0].class_name, "GameObject");
    assert_eq!(parsed_classes[1].class_name, "Transform");

    println!("YamlDocument YAML:\n{}", yaml_content);
}

/// Test serialization with special characters and edge cases
#[test]
fn test_serialize_special_cases() {
    let mut test_class = UnityClass::new(114, "MonoBehaviour".to_string(), "special".to_string());

    // Test various string types
    test_class.set(
        "empty_string".to_string(),
        UnityValue::String("".to_string()),
    );
    test_class.set(
        "quoted_string".to_string(),
        UnityValue::String("Hello \"World\"".to_string()),
    );
    test_class.set(
        "multiline_string".to_string(),
        UnityValue::String("Line 1\nLine 2".to_string()),
    );
    test_class.set(
        "special_chars".to_string(),
        UnityValue::String("Special: []{},".to_string()),
    );

    // Test edge case numbers
    test_class.set("zero_int".to_string(), UnityValue::Integer(0));
    test_class.set("negative_int".to_string(), UnityValue::Integer(-42));
    test_class.set("zero_float".to_string(), UnityValue::Float(0.0));
    test_class.set(
        "negative_float".to_string(),
        UnityValue::Float(-std::f64::consts::PI),
    );

    // Test empty collections
    test_class.set("empty_array".to_string(), UnityValue::Array(vec![]));
    test_class.set(
        "empty_object".to_string(),
        UnityValue::Object(indexmap::IndexMap::new()),
    );

    // Test null value
    test_class.set("null_value".to_string(), UnityValue::Null);

    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serializer.serialize_to_string(&[test_class]).unwrap();

    // Verify special cases are handled
    assert!(yaml_output.contains("empty_string:"));
    assert!(yaml_output.contains("quoted_string:"));
    assert!(yaml_output.contains("empty_array: []"));
    assert!(yaml_output.contains("empty_object: {}"));
    assert!(yaml_output.contains("null_value: {fileID: 0}"));

    // Test that it can be parsed back
    let loader = SerdeUnityLoader::new();
    let parsed_classes = loader.load_from_str(&yaml_output).unwrap();
    assert_eq!(parsed_classes.len(), 1);

    println!("Special cases YAML:\n{}", yaml_output);
}
