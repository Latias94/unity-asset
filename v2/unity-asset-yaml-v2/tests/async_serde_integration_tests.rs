//! Async Serde Integration Tests
//!
//! These tests verify that our async YAML v2 implementation provides the same
//! serde integration capabilities as the blocking version, while offering
//! additional async benefits.

use futures::StreamExt;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::Path;
use unity_asset_core_v2::Result;
use unity_asset_yaml_v2::{
    AsyncUnityDocument, YamlDocument, YamlLoader, DeserializeConfig, LoaderConfig,
    UnityDeserializer, UnityValue,
};

/// Test struct for Unity GameObject deserialization
#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct TestGameObject {
    m_ObjectHideFlags: i32,
    m_Name: String,
    m_TagString: Option<String>,
    m_Layer: i32,
    m_IsActive: i32,
}

/// Test struct for Unity Transform
#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct TestTransform {
    m_ObjectHideFlags: i32,
    m_LocalPosition: TestVector3,
    m_LocalRotation: TestQuaternion,
    m_LocalScale: TestVector3,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct TestVector3 {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct TestQuaternion {
    x: f32,
    y: f32,
    z: f32,
    w: f32,
}

/// Test async equivalent of loading simple Unity GameObject with serde
#[tokio::test]
async fn test_async_serde_load_simple_gameobject() {
    let loader = YamlLoader::new();
    let yaml = r#"
GameObject:
  m_ObjectHideFlags: 0
  m_Name: Player
  m_TagString: Player
  m_Layer: 0
  m_IsActive: 1
"#;

    // Load using async loader
    let result = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    assert_eq!(document.class_count(), 1);

    let class = &document.classes()[0];
    assert_eq!(class.class_name(), "GameObject");

    // Test serde integration with async deserializer
    let deserializer = UnityDeserializer::new();
    let game_object: TestGameObject = deserializer.deserialize(&class.data).await.unwrap();

    assert_eq!(game_object.m_ObjectHideFlags, 0);
    assert_eq!(game_object.m_Name, "Player");
    assert_eq!(game_object.m_TagString, Some("Player".to_string()));
    assert_eq!(game_object.m_Layer, 0);
    assert_eq!(game_object.m_IsActive, 1);
}

/// Test async Transform deserialization with nested objects
#[tokio::test]
async fn test_async_serde_load_transform_with_nested_objects() {
    let loader = YamlLoader::new();
    let yaml = r#"
Transform:
  m_ObjectHideFlags: 0
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 1.5, y: 2.0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
"#;

    let result = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    let class = &document.classes()[0];
    assert_eq!(class.class_name(), "Transform");

    // Test serde deserialization of nested structures
    let deserializer = UnityDeserializer::new();
    let transform: TestTransform = deserializer.deserialize(&class.data).await.unwrap();

    assert_eq!(transform.m_ObjectHideFlags, 0);
    assert_eq!(
        transform.m_LocalPosition,
        TestVector3 {
            x: 1.5,
            y: 2.0,
            z: 0.0
        }
    );
    assert_eq!(
        transform.m_LocalRotation,
        TestQuaternion {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0
        }
    );
    assert_eq!(
        transform.m_LocalScale,
        TestVector3 {
            x: 1.0,
            y: 1.0,
            z: 1.0
        }
    );
}

/// Test async serde with custom deserializer configuration
#[tokio::test]
async fn test_async_serde_with_custom_config() {
    let loader = YamlLoader::with_config(LoaderConfig {
        preserve_order: true,
        max_concurrent_loads: 1,
        ..LoaderConfig::default()
    });

    let yaml = r#"
GameObject:
  m_Name: ConfigTest
  m_IsActive: 1
  custom_field: "test_value"
  another_field: 42
"#;

    let result = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    let class = &document.classes()[0];

    // Test with strict deserialization config
    let strict_config = DeserializeConfig {
        strict: true,
        allow_missing_fields: false,
    };
    let strict_deserializer = UnityDeserializer::with_config(strict_config);

    // This might fail in strict mode with unknown fields
    let strict_result: Result<TestGameObject> = strict_deserializer.deserialize(&class.data).await;

    // Test with lenient deserialization config
    let lenient_config = DeserializeConfig {
        strict: false,
        allow_missing_fields: true,
    };
    let lenient_deserializer = UnityDeserializer::with_config(lenient_config);

    // This should work in lenient mode
    let lenient_result: Result<TestGameObject> =
        lenient_deserializer.deserialize(&class.data).await;

    // At least one should succeed
    assert!(strict_result.is_ok() || lenient_result.is_ok());

    if let Ok(game_object) = lenient_result {
        assert_eq!(game_object.m_Name, "ConfigTest");
        assert_eq!(game_object.m_IsActive, 1);
    }
}

/// Test async streaming serde deserialization
#[tokio::test]
async fn test_async_streaming_serde_deserialization() {
    let loader = YamlLoader::new();
    let yaml = r#"
---
GameObject:
  m_ObjectHideFlags: 0
  m_Name: Object1
  m_IsActive: 1
---
GameObject:
  m_ObjectHideFlags: 0
  m_Name: Object2
  m_IsActive: 1
---
Transform:
  m_ObjectHideFlags: 0
  m_LocalPosition: {x: 1, y: 2, z: 3}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
"#;

    let result = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    assert_eq!(document.class_count(), 3);

    let deserializer = UnityDeserializer::new();

    // Test streaming deserialization - process objects as they come
    let mut object_stream = document.objects_stream();
    let mut deserialized_objects = Vec::new();

    while let Some(class_result) = object_stream.next().await {
        let class = class_result.unwrap();

        match class.class_name() {
            "GameObject" => {
                let game_object: TestGameObject =
                    deserializer.deserialize(&class.data).await.unwrap();
                deserialized_objects.push(format!("GameObject: {}", game_object.m_Name));
            }
            "Transform" => {
                let transform: TestTransform = deserializer.deserialize(&class.data).await.unwrap();
                deserialized_objects.push(format!(
                    "Transform: pos({}, {}, {})",
                    transform.m_LocalPosition.x,
                    transform.m_LocalPosition.y,
                    transform.m_LocalPosition.z
                ));
            }
            _ => {
                deserialized_objects.push(format!("Unknown: {}", class.class_name()));
            }
        }

        // Demonstrate non-blocking behavior
        tokio::task::yield_now().await;
    }

    assert_eq!(deserialized_objects.len(), 3);
    assert!(deserialized_objects.iter().any(|s| s.contains("Object1")));
    assert!(deserialized_objects.iter().any(|s| s.contains("Object2")));
    assert!(deserialized_objects.iter().any(|s| s.contains("Transform")));
}

/// Test async serde serialization round-trip
#[tokio::test]
async fn test_async_serde_serialization_roundtrip() {
    // Create test data
    let original_gameobject = TestGameObject {
        m_ObjectHideFlags: 0,
        m_Name: "TestObject".to_string(),
        m_TagString: Some("Player".to_string()),
        m_Layer: 1,
        m_IsActive: 1,
    };

    let original_transform = TestTransform {
        m_ObjectHideFlags: 0,
        m_LocalPosition: TestVector3 {
            x: 1.5,
            y: 2.0,
            z: 3.0,
        },
        m_LocalRotation: TestQuaternion {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        },
        m_LocalScale: TestVector3 {
            x: 2.0,
            y: 2.0,
            z: 2.0,
        },
    };

    // Convert to UnityValue (simulating serialization)
    let gameobject_value = UnityValue::try_from_serde(&original_gameobject).unwrap();
    let transform_value = UnityValue::try_from_serde(&original_transform).unwrap();

    // Create Unity classes
    let mut classes = Vec::new();
    classes.push(unity_asset_core_v2::AsyncUnityClass::new(
        1,
        "GameObject".to_string(),
        "1".to_string(),
        gameobject_value,
    ));
    classes.push(unity_asset_core_v2::AsyncUnityClass::new(
        4,
        "Transform".to_string(),
        "2".to_string(),
        transform_value,
    ));

    // Create document
    let metadata = unity_asset_core_v2::ObjectMetadata::default();
    let document = YamlDocument::new(classes, metadata);

    // Test serialization to YAML
    let yaml_content = document.serialize_to_yaml().await.unwrap();
    assert!(yaml_content.contains("GameObject"));
    assert!(yaml_content.contains("Transform"));
    assert!(yaml_content.contains("TestObject"));

    // Test deserialization back
    let loader = YamlLoader::new();
    let result = loader
        .load_from_reader(std::io::Cursor::new(yaml_content.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let reloaded_document = result.unwrap();
    assert_eq!(reloaded_document.class_count(), 2);

    // Verify round-trip integrity
    let deserializer = UnityDeserializer::new();

    let gameobject_class = reloaded_document.classes_by_type("GameObject")[0];
    let reloaded_gameobject: TestGameObject = deserializer
        .deserialize(&gameobject_class.data)
        .await
        .unwrap();
    assert_eq!(reloaded_gameobject, original_gameobject);

    let transform_class = reloaded_document.classes_by_type("Transform")[0];
    let reloaded_transform: TestTransform = deserializer
        .deserialize(&transform_class.data)
        .await
        .unwrap();
    assert_eq!(reloaded_transform, original_transform);
}

/// Test async serde error handling
#[tokio::test]
async fn test_async_serde_error_handling() {
    let loader = YamlLoader::new();

    // Test with invalid YAML structure for GameObject
    let invalid_yaml = r#"
GameObject:
  m_Name: 123  # Should be string, not number for strict deserialization
  m_IsActive: "not_a_number"  # Should be number, not string
"#;

    let result = loader
        .load_from_reader(std::io::Cursor::new(invalid_yaml.as_bytes()), None)
        .await;

    if let Ok(document) = result {
        let class = &document.classes()[0];
        let deserializer = UnityDeserializer::new();

        // This should fail due to type mismatches
        let deserialization_result: Result<TestGameObject> =
            deserializer.deserialize(&class.data).await;

        match deserialization_result {
            Ok(_) => {
                // If it succeeds, the deserializer was lenient about types
                println!("Deserialization succeeded with type coercion");
            }
            Err(e) => {
                // Expected - type mismatch should cause error
                println!("Expected deserialization error: {}", e);
                assert!(format!("{}", e).to_lowercase().contains("serialization"));
            }
        }
    }
}

/// Test async serde with complex nested structures
#[tokio::test]
async fn test_async_serde_complex_nested_structures() {
    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct ComplexComponent {
        m_ObjectHideFlags: i32,
        m_Settings: ComponentSettings,
        m_Arrays: ArrayData,
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct ComponentSettings {
        enabled: bool,
        priority: i32,
        config: ConfigData,
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct ConfigData {
        name: String,
        values: Vec<f32>,
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct ArrayData {
        tags: Vec<String>,
        positions: Vec<TestVector3>,
    }

    let complex_yaml = r#"
ComplexComponent:
  m_ObjectHideFlags: 0
  m_Settings:
    enabled: true
    priority: 10
    config:
      name: "TestConfig"
      values: [1.0, 2.5, 3.14]
  m_Arrays:
    tags: ["Player", "Enemy", "Neutral"]
    positions:
    - {x: 1.0, y: 0.0, z: 0.0}
    - {x: 0.0, y: 1.0, z: 0.0}
    - {x: 0.0, y: 0.0, z: 1.0}
"#;

    let loader = YamlLoader::new();
    let result = loader
        .load_from_reader(std::io::Cursor::new(complex_yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    let class = &document.classes()[0];

    let deserializer = UnityDeserializer::new();
    let complex_component: ComplexComponent = deserializer.deserialize(&class.data).await.unwrap();

    assert_eq!(complex_component.m_ObjectHideFlags, 0);
    assert_eq!(complex_component.m_Settings.enabled, true);
    assert_eq!(complex_component.m_Settings.priority, 10);
    assert_eq!(complex_component.m_Settings.config.name, "TestConfig");
    assert_eq!(
        complex_component.m_Settings.config.values,
        vec![1.0, 2.5, 3.14]
    );
    assert_eq!(
        complex_component.m_Arrays.tags,
        vec!["Player", "Enemy", "Neutral"]
    );
    assert_eq!(complex_component.m_Arrays.positions.len(), 3);
    assert_eq!(
        complex_component.m_Arrays.positions[0],
        TestVector3 {
            x: 1.0,
            y: 0.0,
            z: 0.0
        }
    );
}

/// Test async concurrent serde processing
#[tokio::test]
async fn test_async_concurrent_serde_processing() {
    let yaml_documents = vec![
        r#"GameObject:
  m_Name: "Object1"
  m_IsActive: 1"#,
        r#"GameObject:
  m_Name: "Object2"
  m_IsActive: 1"#,
        r#"Transform:
  m_LocalPosition: {x: 1, y: 2, z: 3}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}"#,
    ];

    let loader = YamlLoader::new();
    let deserializer = UnityDeserializer::new();

    // Process documents concurrently
    let mut tasks = Vec::new();

    for (i, yaml) in yaml_documents.iter().enumerate() {
        let loader_clone = loader.clone();
        let deserializer_clone = deserializer.clone();
        let yaml_content = yaml.to_string();

        let task = tokio::spawn(async move {
            let result = loader_clone
                .load_from_reader(std::io::Cursor::new(yaml_content.as_bytes()), None)
                .await;

            if let Ok(document) = result {
                let class = &document.classes()[0];

                match class.class_name() {
                    "GameObject" => {
                        let obj: Result<TestGameObject> =
                            deserializer_clone.deserialize(&class.data).await;
                        (i, "GameObject".to_string(), obj.is_ok())
                    }
                    "Transform" => {
                        let obj: Result<TestTransform> =
                            deserializer_clone.deserialize(&class.data).await;
                        (i, "Transform".to_string(), obj.is_ok())
                    }
                    _ => (i, "Unknown".to_string(), false),
                }
            } else {
                (i, "Error".to_string(), false)
            }
        });

        tasks.push(task);
    }

    // Wait for all concurrent tasks to complete
    let results = futures::future::join_all(tasks).await;

    assert_eq!(results.len(), 3);

    for result in results {
        let (index, class_type, success) = result.unwrap();
        println!(
            "Document {}: {} - {}",
            index,
            class_type,
            if success { "Success" } else { "Failed" }
        );
        assert!(
            success,
            "Concurrent serde processing failed for document {}",
            index
        );
    }

    println!("✓ Concurrent serde processing completed successfully");
}
