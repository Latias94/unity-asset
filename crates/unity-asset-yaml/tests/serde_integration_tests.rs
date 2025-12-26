//! Integration tests for the serde-based Unity YAML loader
//!
//! These tests verify that our serde-based implementation can handle
//! real Unity YAML files correctly.

use std::path::Path;
use unity_asset_core::{UnityDocument, UnityValue};
use unity_asset_yaml::{SerdeUnityLoader, YamlDocument};

/// Test loading simple Unity GameObject YAML
#[test]
fn test_load_simple_gameobject() {
    let loader = SerdeUnityLoader::new();
    let yaml = r#"
GameObject:
  m_ObjectHideFlags: 0
  m_Name: Player
  m_TagString: Player
  m_Layer: 0
  m_IsActive: 1
"#;

    let result = loader.load_from_str(yaml);
    assert!(result.is_ok());

    let classes = result.unwrap();
    assert_eq!(classes.len(), 1);

    let class = &classes[0];
    assert_eq!(class.class_name, "GameObject");

    if let Some(UnityValue::String(name)) = class.get("m_Name") {
        assert_eq!(name, "Player");
    } else {
        panic!("Expected m_Name property");
    }

    if let Some(UnityValue::String(tag)) = class.get("m_TagString") {
        assert_eq!(tag, "Player");
    } else {
        panic!("Expected m_TagString property");
    }
}

/// Test loading Unity Transform with nested objects
#[test]
fn test_load_transform_with_nested_objects() {
    let loader = SerdeUnityLoader::new();
    let yaml = r#"
Transform:
  m_ObjectHideFlags: 0
  m_GameObject: {fileID: 123456789}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 1.5, y: 2.0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Children: []
"#;

    let result = loader.load_from_str(yaml);
    assert!(result.is_ok());

    let classes = result.unwrap();
    assert_eq!(classes.len(), 1);

    let class = &classes[0];
    assert_eq!(class.class_name, "Transform");

    // Check nested object properties
    if let Some(UnityValue::Object(pos)) = class.get("m_LocalPosition") {
        if let Some(UnityValue::Float(x)) = pos.get("x") {
            assert_eq!(*x, 1.5);
        } else {
            panic!("Expected x coordinate");
        }
        if let Some(UnityValue::Float(y)) = pos.get("y") {
            assert_eq!(*y, 2.0);
        } else {
            panic!("Expected y coordinate");
        }
    } else {
        panic!("Expected m_LocalPosition property");
    }
}

/// Test loading Unity MonoBehaviour with arrays
#[test]
fn test_load_monobehaviour_with_arrays() {
    let loader = SerdeUnityLoader::new();
    let yaml = r#"
MonoBehaviour:
  m_ObjectHideFlags: 0
  m_GameObject: {fileID: 123456789}
  m_Enabled: 1
  m_Script: {fileID: 11500000, guid: abc123def456, type: 3}
  m_Components:
  - {fileID: 111}
  - {fileID: 222}
  - {fileID: 333}
  m_Tags:
  - Player
  - Enemy
  - Collectible
  customValues:
  - 1.0
  - 2.5
  - 3.14
"#;

    let result = loader.load_from_str(yaml);
    assert!(result.is_ok());

    let classes = result.unwrap();
    assert_eq!(classes.len(), 1);

    let class = &classes[0];
    assert_eq!(class.class_name, "MonoBehaviour");

    // Check array properties
    if let Some(UnityValue::Array(components)) = class.get("m_Components") {
        assert_eq!(components.len(), 3);
    } else {
        panic!("Expected m_Components array");
    }

    if let Some(UnityValue::Array(tags)) = class.get("m_Tags") {
        assert_eq!(tags.len(), 3);
        if let UnityValue::String(first_tag) = &tags[0] {
            assert_eq!(first_tag, "Player");
        } else {
            panic!("Expected first tag to be Player");
        }
    } else {
        panic!("Expected m_Tags array");
    }

    if let Some(UnityValue::Array(values)) = class.get("customValues") {
        assert_eq!(values.len(), 3);
        if let UnityValue::Float(first_val) = &values[0] {
            assert_eq!(*first_val, 1.0);
        } else {
            panic!("Expected first value to be 1.0");
        }
    } else {
        panic!("Expected customValues array");
    }
}

/// Test loading multiple documents
#[test]
fn test_load_multiple_documents() {
    let loader = SerdeUnityLoader::new();
    let yaml = r#"
---
GameObject:
  m_Name: Object1
  m_IsActive: 1
---
Transform:
  m_LocalPosition: {x: 0, y: 0, z: 0}
---
MonoBehaviour:
  m_Enabled: 1
  customProperty: 42
"#;

    let result = loader.load_from_str(yaml);
    assert!(result.is_ok());

    let classes = result.unwrap();
    assert_eq!(classes.len(), 3);

    // Verify we got the expected classes
    assert_eq!(classes[0].class_name, "GameObject");
    assert_eq!(classes[1].class_name, "Transform");
    assert_eq!(classes[2].class_name, "MonoBehaviour");
}

/// Test YamlDocument file loading
#[test]
fn test_yaml_document_file_loading() {
    let fixture_path = Path::new("tests/fixtures/simple_gameobject.yaml");

    // Only run this test if the fixture file exists
    if fixture_path.exists() {
        let result = YamlDocument::load_yaml(fixture_path, false);

        match result {
            Ok(doc) => {
                // Should have one entry
                assert_eq!(doc.entries().len(), 1);

                let entry = &doc.entries()[0];
                assert_eq!(entry.class_name, "GameObject");

                // Check properties
                if let Some(UnityValue::String(name)) = entry.get("m_Name") {
                    assert_eq!(name, "Player");
                } else {
                    panic!("Expected m_Name property");
                }
            }
            Err(e) => {
                panic!("Failed to load YAML file: {}", e);
            }
        }
    }
}

/// Test error handling with invalid YAML
#[test]
fn test_error_handling() {
    let loader = SerdeUnityLoader::new();
    let invalid_yaml = "invalid: yaml: content: [unclosed";

    let result = loader.load_from_str(invalid_yaml);
    assert!(result.is_err());

    // Verify we get a meaningful error message
    let error = result.unwrap_err();
    let error_msg = format!("{}", error);
    assert!(error_msg.contains("YAML parsing error"));
}

/// Test empty YAML handling
#[test]
fn test_empty_yaml() {
    let loader = SerdeUnityLoader::new();
    let empty_yaml = "";

    let result = loader.load_from_str(empty_yaml);
    assert!(result.is_ok());

    let classes = result.unwrap();
    // Empty YAML creates one scalar document
    assert_eq!(classes.len(), 1);
    assert_eq!(classes[0].class_name, "Scalar");
}

/// Test YAML with only whitespace
#[test]
fn test_whitespace_only_yaml() {
    let loader = SerdeUnityLoader::new();
    let whitespace_yaml = "   \n  \t  \n  ";

    let result = loader.load_from_str(whitespace_yaml);
    // Whitespace-only YAML causes a parsing error
    assert!(result.is_err());

    let error = result.unwrap_err();
    let error_msg = format!("{}", error);
    assert!(error_msg.contains("YAML parsing error"));
}
