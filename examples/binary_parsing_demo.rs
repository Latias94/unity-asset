//! Unity Binary Parsing Demo
//!
//! This example demonstrates how to parse Unity binary files including
//! AssetBundles and SerializedFiles using the unity-asset-binary crate.

use std::fs;
use std::path::Path;
use unity_asset_binary::{AssetBundle, BinaryError, SerializedFile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🎮 Unity Binary Parsing Demo");
    println!("============================");

    // Demo 1: AssetBundle parsing
    demo_assetbundle_parsing()?;

    // Demo 2: SerializedFile parsing
    demo_serialized_file_parsing()?;

    // Demo 3: Error handling
    demo_error_handling()?;

    println!("\n✅ All demos completed successfully!");
    Ok(())
}

/// Demonstrate AssetBundle parsing
fn demo_assetbundle_parsing() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n📦 Demo 1: AssetBundle Parsing");
    println!("------------------------------");

    // Create a mock AssetBundle for demonstration
    // In real usage, you would load from a file:
    // let data = fs::read("path/to/bundle.unity3d")?;

    let mock_bundle_data = create_mock_assetbundle_data();

    match AssetBundle::from_bytes(mock_bundle_data) {
        Ok(bundle) => {
            println!("✓ Successfully parsed AssetBundle");
            println!("  Unity Version: {}", bundle.unity_version());
            println!("  Bundle Name: {}", bundle.name());
            println!("  File Count: {}", bundle.file_names().len());

            // List all files in the bundle
            for file_name in bundle.file_names() {
                println!("    - {}", file_name);
            }

            // Access contained assets
            println!("  Asset Count: {}", bundle.assets().len());
            for (i, asset) in bundle.assets().iter().enumerate() {
                println!(
                    "    Asset {}: {} (Unity {})",
                    i + 1,
                    asset.name(),
                    asset.unity_version()
                );
            }
        }
        Err(e) => {
            println!(
                "⚠ AssetBundle parsing failed (expected for mock data): {}",
                e
            );
            println!("  This is normal since we're using mock data");
        }
    }

    Ok(())
}

/// Demonstrate SerializedFile parsing
fn demo_serialized_file_parsing() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n📄 Demo 2: SerializedFile Parsing");
    println!("----------------------------------");

    // Create a mock SerializedFile for demonstration
    let mock_asset_data = create_mock_serialized_file_data();

    match SerializedFile::from_bytes(mock_asset_data) {
        Ok(asset) => {
            println!("✓ Successfully parsed SerializedFile");
            println!("  Unity Version: {}", asset.unity_version());
            println!("  Target Platform: {}", asset.target_platform());
            println!("  File Name: {}", asset.name());

            // Try to get objects from the asset
            match asset.get_objects() {
                Ok(objects) => {
                    println!("  Object Count: {}", objects.len());
                    for (i, object) in objects.iter().enumerate() {
                        println!(
                            "    Object {}: {} (Class: {}, ID: {})",
                            i + 1,
                            object.name().unwrap_or_else(|| "<unnamed>".to_string()),
                            object.class_name(),
                            object.path_id()
                        );
                    }
                }
                Err(e) => {
                    println!("  ⚠ Could not extract objects: {}", e);
                }
            }
        }
        Err(e) => {
            println!(
                "⚠ SerializedFile parsing failed (expected for mock data): {}",
                e
            );
            println!("  This is normal since we're using mock data");
        }
    }

    Ok(())
}

/// Demonstrate error handling
fn demo_error_handling() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n🚨 Demo 3: Error Handling");
    println!("-------------------------");

    // Test various error conditions
    let test_cases = vec![
        ("Empty data", Vec::new()),
        ("Invalid signature", b"INVALID_SIGNATURE".to_vec()),
        ("Truncated data", b"UnityFS\x00\x00\x00\x06".to_vec()),
    ];

    for (description, data) in test_cases {
        println!("  Testing: {}", description);

        match AssetBundle::from_bytes(data.clone()) {
            Ok(_) => println!("    ✓ Unexpectedly succeeded"),
            Err(e) => match e {
                BinaryError::InvalidFormat(msg) => {
                    println!("    ✓ Invalid format error: {}", msg);
                }
                BinaryError::InvalidSignature { expected, actual } => {
                    println!(
                        "    ✓ Invalid signature error: expected '{}', got '{}'",
                        expected, actual
                    );
                }
                BinaryError::NotEnoughData { expected, actual } => {
                    println!(
                        "    ✓ Not enough data error: expected {}, got {}",
                        expected, actual
                    );
                }
                other => {
                    println!("    ✓ Other error: {}", other);
                }
            },
        }

        // Also test SerializedFile parsing
        match SerializedFile::from_bytes(data) {
            Ok(_) => println!("    ✓ SerializedFile unexpectedly succeeded"),
            Err(_) => println!("    ✓ SerializedFile correctly failed"),
        }
    }

    Ok(())
}

/// Create mock AssetBundle data for demonstration
/// In real usage, this would be loaded from an actual Unity AssetBundle file
fn create_mock_assetbundle_data() -> Vec<u8> {
    // This is a simplified mock - real AssetBundle files are much more complex
    let mut data = Vec::new();

    // Mock UnityFS signature
    data.extend_from_slice(b"UnityFS\0");

    // Mock version (6)
    data.extend_from_slice(&6u32.to_be_bytes());

    // Mock Unity version
    data.extend_from_slice(b"2019.4.0f1\0");

    // Mock Unity revision
    data.extend_from_slice(b"abc123def456\0");

    // Mock file size
    data.extend_from_slice(&1000i64.to_be_bytes());

    // Mock compressed blocks info size
    data.extend_from_slice(&100u32.to_be_bytes());

    // Mock uncompressed blocks info size
    data.extend_from_slice(&200u32.to_be_bytes());

    // Mock flags (no compression)
    data.extend_from_slice(&0u32.to_be_bytes());

    // Add some padding to make it look more realistic
    data.resize(200, 0);

    data
}

/// Create mock SerializedFile data for demonstration
/// In real usage, this would be loaded from an actual Unity asset file
fn create_mock_serialized_file_data() -> Vec<u8> {
    let mut data = Vec::new();

    // Mock SerializedFile header
    data.extend_from_slice(&100u32.to_le_bytes()); // metadata_size
    data.extend_from_slice(&1000u32.to_le_bytes()); // file_size
    data.extend_from_slice(&15u32.to_le_bytes()); // version
    data.extend_from_slice(&200u32.to_le_bytes()); // data_offset
    data.push(0); // endian (little)
    data.extend_from_slice(&[0, 0, 0]); // reserved

    // Add some mock metadata
    data.extend_from_slice(b"2019.4.0f1\0"); // Unity version
    data.extend_from_slice(&5i32.to_le_bytes()); // target platform
    data.push(1); // enable_type_tree

    // Mock type count (0 for simplicity)
    data.extend_from_slice(&0u32.to_le_bytes());

    // Mock object count (0 for simplicity)
    data.extend_from_slice(&0u32.to_le_bytes());

    // Mock external count (0 for simplicity)
    data.extend_from_slice(&0u32.to_le_bytes());

    // Add padding to reach the expected size
    data.resize(1000, 0);

    data
}

/// Helper function to check if a file exists and is readable
fn _check_file_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Helper function to get file size
fn _get_file_size(path: &str) -> Result<u64, std::io::Error> {
    let metadata = fs::metadata(path)?;
    Ok(metadata.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_data_creation() {
        let bundle_data = create_mock_assetbundle_data();
        assert!(!bundle_data.is_empty());
        assert!(bundle_data.starts_with(b"UnityFS"));

        let asset_data = create_mock_serialized_file_data();
        assert!(!asset_data.is_empty());
        assert_eq!(asset_data.len(), 1000);
    }

    #[test]
    fn test_error_handling_demo() {
        // This should not panic
        let result = demo_error_handling();
        assert!(result.is_ok());
    }
}
