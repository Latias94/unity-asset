//! Simple Blocking API Compatibility Tests
//!
//! Simplified tests to verify async v2 replicates core blocking API functionality

use futures::StreamExt;
use unity_asset_core_v2::UnityValue;
use unity_asset_yaml_v2::{AsyncUnityDocument, YamlDocument, YamlLoader};

/// Test basic GameObject loading - mirrors blocking SerdeUnityLoader::load_from_str
#[tokio::test]
async fn test_load_simple_gameobject() {
    let loader = YamlLoader::new();
    let yaml = r#"
GameObject:
  m_Name: Player
  m_IsActive: 1
"#;

    // Load using async loader - same pattern as blocking version
    let document = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await
        .unwrap();
    assert_eq!(document.class_count(), 1);

    let class = &document.classes()[0];
    assert_eq!(class.class_name(), "GameObject");
    assert_eq!(class.name(), Some("Player".to_string()));
}

/// Test Transform with nested objects - mirrors blocking nested object test
#[tokio::test]
async fn test_load_transform_nested() {
    let loader = YamlLoader::new();
    let yaml = r#"
Transform:
  m_LocalPosition: {x: 1.5, y: 2.0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
"#;

    let document = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await
        .unwrap();
    let class = &document.classes()[0];
    assert_eq!(class.class_name(), "Transform");

    // Test nested object access
    if let Some(UnityValue::Object(pos)) = class.get_property("m_LocalPosition") {
        assert!(pos.get("x").is_some());
        assert!(pos.get("y").is_some());
        assert!(pos.get("z").is_some());
    }
}

/// Test multiple documents - mirrors blocking multi-doc test
#[tokio::test]
async fn test_load_multiple_documents() {
    let loader = YamlLoader::new();
    let yaml = r#"
---
GameObject:
  m_Name: Object1
---
Transform:
  m_LocalPosition: {x: 0, y: 0, z: 0}
---
MonoBehaviour:
  m_Enabled: 1
"#;

    let document = loader
        .load_from_reader(std::io::Cursor::new(yaml.as_bytes()), None)
        .await
        .unwrap();
    assert_eq!(document.class_count(), 3);

    let classes = document.classes();
    assert_eq!(classes[0].class_name(), "GameObject");
    assert_eq!(classes[1].class_name(), "Transform");
    assert_eq!(classes[2].class_name(), "MonoBehaviour");
}

/// Test streaming - new async capability beyond blocking version
#[tokio::test]
async fn test_async_streaming() {
    let yaml = r#"
---
GameObject:
  m_Name: Object1
---
GameObject:
  m_Name: Object2
"#;

    let document = YamlDocument::load_from_stream(std::io::Cursor::new(yaml.as_bytes()))
        .await
        .unwrap();

    let mut stream = document.objects_stream();
    let mut count = 0;

    while let Some(class_result) = stream.next().await {
        let class = class_result.unwrap();
        assert_eq!(class.class_name(), "GameObject");
        count += 1;
    }

    assert_eq!(count, 2);
}

/// Test error handling - mirrors blocking error test
#[tokio::test]
async fn test_error_handling() {
    let loader = YamlLoader::new();
    let invalid_yaml = "invalid: yaml: [unclosed";

    let result = loader
        .load_from_reader(std::io::Cursor::new(invalid_yaml.as_bytes()), None)
        .await;
    assert!(result.is_err());
}

/// Test concurrent loading - async advantage over blocking
#[tokio::test]
async fn test_concurrent_loading() {
    use unity_asset_core_v2::AsyncAssetLoader;
    use unity_asset_yaml_v2::async_loader::LoaderConfig;

    let yaml_content = r#"GameObject:
  m_Name: TestObject"#;

    // Create temp files
    let temp_files = (0..3)
        .map(|_i| {
            let mut temp_file = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut temp_file, yaml_content.as_bytes()).unwrap();
            temp_file
        })
        .collect::<Vec<_>>();

    let loader = YamlLoader::new();
    let paths: Vec<_> = temp_files.iter().map(|f| f.path().to_path_buf()).collect();

    let stream = loader.load_assets(paths, LoaderConfig::default()).await;
    tokio::pin!(stream);

    let mut count = 0;
    while let Some(result) = stream.next().await {
        assert!(result.is_ok());
        count += 1;
    }

    assert_eq!(count, 3);
}
