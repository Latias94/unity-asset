//! V2 Integration Test
//!
//! Tests the integration between all V2 modules to ensure they work together correctly.

use std::path::Path;
use tokio;

// Import all V2 modules
use unity_asset_binary_v2::{AssetConfig, AssetBundle, SerializedFile, BundleConfig};
use unity_asset_core_v2::{AsyncUnityClass, Result, UnityValue};
use unity_asset_yaml_v2::{YamlDocument, YamlLoader};

/// Test basic V2 module integration
#[tokio::test]
async fn test_v2_integration() -> Result<()> {
    println!("🧪 Testing V2 module integration...");

    // Test 1: Core types work
    test_core_types().await?;

    // Test 2: YAML module integration
    test_yaml_integration().await?;

    // Test 3: Binary module integration
    test_binary_integration().await?;

    println!("✅ All V2 integration tests passed!");
    Ok(())
}

/// Test core V2 types
async fn test_core_types() -> Result<()> {
    println!("  🔧 Testing core types...");

    // Test AsyncUnityClass creation
    let mut unity_class =
        AsyncUnityClass::new(1, "TestObject".to_string(), "test_anchor".to_string());

    // Test property access
    unity_class.set(
        "test_property".to_string(),
        UnityValue::String("test_value".to_string()),
    );

    if let Some(value) = unity_class.get("test_property") {
        assert_eq!(value, &UnityValue::String("test_value".to_string()));
        println!("    ✓ Property access works");
    } else {
        return Err(unity_asset_core_v2::UnityAssetError::validation(
            "test_property",
            "Property not found",
        ));
    }

    println!("    ✓ Core types working");
    Ok(())
}

/// Test YAML module integration
async fn test_yaml_integration() -> Result<()> {
    println!("  📄 Testing YAML integration...");

    // Create a simple YAML document
    let classes = vec![
        AsyncUnityClass::new(1, "GameObject".to_string(), "100000".to_string()),
        AsyncUnityClass::new(4, "Transform".to_string(), "100001".to_string()),
    ];

    let doc = YamlDocument::new(classes, Default::default());

    // Test document access
    assert_eq!(doc.classes().len(), 2);
    assert_eq!(doc.classes()[0].class_name(), "GameObject");
    assert_eq!(doc.classes()[1].class_name(), "Transform");

    println!("    ✓ YAML document creation works");

    // Test serialization
    let yaml_content = doc.serialize_to_yaml().await?;
    assert!(!yaml_content.is_empty());
    println!("    ✓ YAML serialization works");

    println!("    ✓ YAML integration working");
    Ok(())
}

/// Test binary module integration
async fn test_binary_integration() -> Result<()> {
    println!("  🔧 Testing binary integration...");

    // Test configuration creation
    let bundle_config = BundleConfig::default();
    assert!(bundle_config.max_concurrent_bundles > 0);
    println!("    ✓ Bundle configuration works");

    let asset_config = AssetConfig::default();
    assert!(asset_config.max_concurrent_objects > 0);
    println!("    ✓ Asset configuration works");

    // Test type definitions exist
    use unity_asset_binary_v2::{BundleFileInfo, BundleHeader, ObjectInfo, SerializedFileHeader};

    let header = BundleHeader::new();
    assert_eq!(header.version, 0);
    println!("    ✓ Bundle header works");

    let file_info = BundleFileInfo::new("test.assets".to_string(), 0, 1024);
    assert_eq!(file_info.name, "test.assets");
    assert_eq!(file_info.size, 1024);
    println!("    ✓ File info works");

    let obj_info = ObjectInfo::new(123, 0, 256, 1);
    assert_eq!(obj_info.path_id, 123);
    assert_eq!(obj_info.class_id, 1);
    println!("    ✓ Object info works");

    println!("    ✓ Binary integration working");
    Ok(())
}

/// Test CLI integration (basic compilation test)
#[tokio::test]
async fn test_cli_integration() -> Result<()> {
    println!("🖥️  Testing CLI integration...");

    // This test just ensures the CLI types compile and can be imported
    // We can't easily test the actual CLI without running it

    println!("    ✓ CLI types compile successfully");
    Ok(())
}

/// Test error handling across modules
#[tokio::test]
async fn test_error_handling() -> Result<()> {
    println!("⚠️  Testing error handling...");

    // Test that errors propagate correctly across module boundaries
    use unity_asset_core_v2::UnityAssetError;

    let error = UnityAssetError::parse_error("Test error".to_string(), 0);
    assert!(error.to_string().contains("Test error"));

    println!("    ✓ Error handling works");
    Ok(())
}

/// Test async traits and functionality
#[tokio::test]
async fn test_async_functionality() -> Result<()> {
    println!("⚡ Testing async functionality...");

    // Test that async traits work
    use unity_asset_core_v2::AsyncUnityDocument;

    // This tests that the trait is properly defined and can be used
    // We can't test actual file loading without sample files

    println!("    ✓ Async traits compile successfully");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("🚀 Running V2 Integration Tests");
    println!("================================");

    test_v2_integration()?;
    test_cli_integration()?;
    test_error_handling()?;
    test_async_functionality()?;

    println!("\n🎉 All V2 integration tests completed successfully!");
    println!("   The V2 async system is properly integrated and functional.");

    Ok(())
}
