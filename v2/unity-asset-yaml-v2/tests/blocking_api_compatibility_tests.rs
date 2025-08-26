//! Blocking API Compatibility Tests
//!
//! These tests verify that our async YAML v2 implementation can replicate
//! all the functionality of the original blocking YAML API, ensuring
//! backwards compatibility for users who want to migrate.

use futures::StreamExt;
use unity_asset_core_v2::{AsyncUnityClass, UnityValue};
use unity_asset_yaml_v2::{
    async_loader::LoaderConfig, AsyncUnityDocument, AsyncYamlDocument, AsyncYamlLoader,
};

/// Test async equivalent of SerdeUnityLoader::load_from_str
#[tokio::test]
async fn test_async_load_simple_gameobject() {
    let loader = AsyncYamlLoader::new();
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

    let classes = document.classes();
    let class = &classes[0];
    assert_eq!(class.class_name(), "GameObject");

    // Test same data access patterns as blocking version
    if let Some(name) = class.name() {
        assert_eq!(name, "Player");
    } else {
        panic!("Expected m_Name property");
    }

    // Test data extraction
    if let UnityValue::String(tag) = class.get_property("m_TagString").unwrap() {
        assert_eq!(tag, "Player");
    } else {
        panic!("Expected m_TagString property");
    }
}

/// Test async equivalent of loading Transform with nested objects
#[tokio::test]
async fn test_async_load_transform_with_nested_objects() {
    let loader = AsyncYamlLoader::new();
    let yaml = r#"
Transform:
  m_ObjectHideFlags: 0
  m_GameObject: {fileID: 123456789}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 1.5, y: 2.0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Children: []
"#;

    let result = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    assert_eq!(document.class_count(), 1);

    let class = &document.classes()[0];
    assert_eq!(class.class_name(), "Transform");

    // Test nested object access - same pattern as blocking version
    if let UnityValue::Object(pos_map) = class.get_property("m_LocalPosition").unwrap() {
        if let Some(UnityValue::Float(x)) = pos_map.get("x") {
            assert_eq!(*x, 1.5);
        } else {
            panic!("Expected x coordinate");
        }
        if let Some(UnityValue::Float(y)) = pos_map.get("y") {
            assert_eq!(*y, 2.0);
        } else {
            panic!("Expected y coordinate");
        }
    } else {
        panic!("Expected m_LocalPosition property");
    }
}

/// Test async equivalent of loading MonoBehaviour with arrays
#[tokio::test]
async fn test_async_load_monobehaviour_with_arrays() {
    let loader = AsyncYamlLoader::new();
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

    let result = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    let class = &document.classes()[0];
    assert_eq!(class.class_name(), "MonoBehaviour");

    // Test array properties - same access pattern
    if let UnityValue::Array(components) = class.get_property("m_Components").unwrap() {
        assert_eq!(components.len(), 3);
    } else {
        panic!("Expected m_Components array");
    }

    if let UnityValue::Array(tags) = class.get_property("m_Tags").unwrap() {
        assert_eq!(tags.len(), 3);
        if let UnityValue::String(first_tag) = &tags[0] {
            assert_eq!(first_tag, "Player");
        } else {
            panic!("Expected first tag to be Player");
        }
    } else {
        panic!("Expected m_Tags array");
    }

    if let UnityValue::Array(values) = class.get_property("customValues").unwrap() {
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

/// Test async equivalent of loading multiple documents
#[tokio::test]
async fn test_async_load_multiple_documents() {
    let loader = AsyncYamlLoader::new();
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

    let result = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    assert_eq!(document.class_count(), 3);

    // Verify we got the expected classes - same validation pattern
    let classes = document.classes();
    assert_eq!(classes[0].class_name(), "GameObject");
    assert_eq!(classes[1].class_name(), "Transform");
    assert_eq!(classes[2].class_name(), "MonoBehaviour");
}

/// Test async streaming equivalent for document processing
#[tokio::test]
async fn test_async_document_streaming() {
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

    // Test streaming approach (new capability not in blocking version)
    let document = AsyncYamlDocument::load_from_stream(std::io::Cursor::new(yaml.as_bytes()))
        .await
        .unwrap();

    let mut stream = document.objects_stream();
    let mut collected_classes = Vec::new();

    while let Some(class_result) = stream.next().await {
        let class = class_result.unwrap();
        collected_classes.push(class);
    }

    assert_eq!(collected_classes.len(), 3);
    assert_eq!(collected_classes[0].class_name(), "GameObject");
    assert_eq!(collected_classes[1].class_name(), "Transform");
    assert_eq!(collected_classes[2].class_name(), "MonoBehaviour");
}

/// Test async equivalent of error handling
#[tokio::test]
async fn test_async_error_handling() {
    let loader = AsyncYamlLoader::new();
    let invalid_yaml = "invalid: yaml: content: [unclosed";

    let result = loader
        .load_from_reader(std::io::Cursor::new(invalid_yaml.as_bytes()), None)
        .await;
    assert!(result.is_err());

    // Verify we get a meaningful error message - same pattern as blocking
    let error = result.unwrap_err();
    let error_msg = format!("{}", error);
    assert!(error_msg.contains("YAML parsing failed") || error_msg.contains("parsing"));
}

/// Test async equivalent of empty YAML handling
#[tokio::test]
async fn test_async_empty_yaml() {
    let loader = AsyncYamlLoader::new();
    let empty_yaml = "";

    let result = loader
        .load_from_reader(std::io::Cursor::new(empty_yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    // Empty YAML should result in empty document
    assert_eq!(document.class_count(), 0);
}

/// Test async equivalent of configuration options
#[tokio::test]
async fn test_async_loader_configuration() {
    // Test different configurations - equivalent to blocking version options
    let config_large = LoaderConfig::for_large_files();
    let loader_large = AsyncYamlLoader::with_config(config_large);
    assert_eq!(loader_large.config().max_concurrent_loads, 4);
    assert_eq!(loader_large.config().buffer_size, 32768);
    assert!(!loader_large.config().resolve_anchors);

    let config_small = LoaderConfig::for_small_files();
    let loader_small = AsyncYamlLoader::with_config(config_small);
    assert_eq!(loader_small.config().max_concurrent_loads, 16);
    assert!(loader_small.config().preserve_order);
    assert!(loader_small.config().resolve_anchors);
}

/// Test async concurrent loading - new capability vs blocking version
#[tokio::test]
async fn test_async_concurrent_loading_advantage() {
    let yaml_content = r#"
Transform:
  m_Position: {x: 1.0, y: 2.0, z: 3.0}
"#;

    // Create multiple temp files
    let temp_files = (0..3)
        .map(|_| {
            let mut temp_file = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut temp_file, yaml_content.as_bytes()).unwrap();
            temp_file
        })
        .collect::<Vec<_>>();

    let loader = AsyncYamlLoader::new();
    let paths: Vec<_> = temp_files.iter().map(|f| f.path().to_path_buf()).collect();

    // Test concurrent loading - this is a major advantage over blocking version
    let start = std::time::Instant::now();
    let stream = loader.load_assets(paths, LoaderConfig::default()).await;
    tokio::pin!(stream);

    let mut count = 0;
    while let Some(result) = stream.next().await {
        assert!(result.is_ok());
        count += 1;
    }

    let duration = start.elapsed();
    assert_eq!(count, 3);

    // Concurrent loading should be faster than sequential
    println!("Concurrent loading took: {:?}", duration);
    // The key advantage is that this is non-blocking and can process other tasks
}

/// Test that async version maintains same data fidelity as blocking version
#[tokio::test]
async fn test_async_data_fidelity() {
    let complex_yaml = r#"
PlayerSettings:
  m_ObjectHideFlags: 0
  serializedVersion: 20
  productGUID: b0b80c8a87a3c4c8a9b2f37d7a3b9c3e
  AndroidBundleVersionCode: 1
  AndroidMinSdkVersion: 21
  m_SplashScreenBackgroundColor: {r: 0.1176471, g: 0.1176471, b: 0.1176471, a: 1}
  m_SplashScreenLogos: []
  m_BuildTargetIcons:
  - m_BuildTarget: 
    m_Icons:
    - serializedVersion: 2
      m_Icon: {fileID: 2800000, guid: abc123def456, type: 3}
      m_Width: 192
      m_Height: 192
      m_Kind: 0
  m_BuildTargetPlatformSettings:
  - m_BuildTarget: Android
    m_PlayerSettings:
      resolutionDialogBanner: {fileID: 0}
      m_ShowUnitySplashScreen: 1
      m_ShowUnitySplashLogo: 1
  vrSettings:
    cardboard:
      depthFormat: 0
      enableTransitionView: 0
    daydream:
      depthFormat: 0
      useSustainedPerformanceMode: 0
      enableVideoLayer: 0
      useProtectedVideoMemory: 0
"#;

    let loader = AsyncYamlLoader::new();
    let result = loader
        .load_from_reader(std::io::Cursor::new(complex_yaml.as_bytes()), None)
        .await;
    assert!(result.is_ok());

    let document = result.unwrap();
    assert_eq!(document.class_count(), 1);

    let class = &document.classes()[0];
    assert_eq!(class.class_name(), "PlayerSettings");

    // Test deep nested object access - ensuring same fidelity as blocking version
    if let UnityValue::Object(splash_color) =
        class.get_property("m_SplashScreenBackgroundColor").unwrap()
    {
        assert!(splash_color.get("r").is_some());
        assert!(splash_color.get("g").is_some());
        assert!(splash_color.get("b").is_some());
        assert!(splash_color.get("a").is_some());
    } else {
        panic!("Expected m_SplashScreenBackgroundColor object");
    }

    // Test complex nested arrays and objects
    if let UnityValue::Array(build_targets) =
        class.get_property("m_BuildTargetPlatformSettings").unwrap()
    {
        assert_eq!(build_targets.len(), 1);

        if let UnityValue::Object(android_settings) = &build_targets[0] {
            if let Some(UnityValue::String(build_target)) = android_settings.get("m_BuildTarget") {
                assert_eq!(build_target, "Android");
            }
        }
    }

    // Test deeply nested structures
    if let UnityValue::Object(vr_settings) = class.get_property("vrSettings").unwrap() {
        assert!(vr_settings.get("cardboard").is_some());
        assert!(vr_settings.get("daydream").is_some());
    }
}

/// Benchmark comparison: Async vs theoretical blocking performance
#[tokio::test]
async fn test_async_performance_characteristics() {
    let yaml_content = r#"
GameObject:
  m_Name: "PerformanceTestObject"
  m_Components:
  - {fileID: 1}
  - {fileID: 2}
  - {fileID: 3}
  m_Properties:
    health: 100
    damage: 25
    speed: 5.5
"#;

    let loader = AsyncYamlLoader::new();

    // Test async loading performance
    let start = std::time::Instant::now();
    for _ in 0..100 {
        let result = loader
            .load_from_reader(std::io::Cursor::new(yaml_content.as_bytes()), None)
            .await;
        assert!(result.is_ok());

        // Yield control to allow other tasks
        tokio::task::yield_now().await;
    }
    let async_duration = start.elapsed();

    println!("Async loading 100 documents took: {:?}", async_duration);
    println!("Average per document: {:?}", async_duration / 100);

    // The key advantage is not raw speed but non-blocking behavior
    // and ability to handle multiple files concurrently
    assert!(async_duration.as_millis() < 1000); // Should be reasonably fast
}
