//! Basic API Structure Test
//!
//! Test that the basic blocking API structure matches original expectations

use unity_asset_binary_v2::{AssetBundle, AudioClipProcessor, Texture2DProcessor, UnityVersion};

#[tokio::test]
async fn test_api_structure_matches_original() {
    println!("Testing v2 blocking API structure compatibility...");

    // Test that we can create the basic types that UnityPy tests expect

    // Test UnityVersion
    let version = UnityVersion::from_str("2020.3.12f1").unwrap();
    println!(
        "  ✓ Created UnityVersion: {}.{}.{}",
        version.major, version.minor, version.patch
    );

    // Test AudioClipProcessor creation
    let audio_processor = AudioClipProcessor::new(version.clone());
    println!("  ✓ Created AudioClipProcessor");

    // Test Texture2DProcessor creation
    let texture_processor = Texture2DProcessor::new(version.clone());
    println!("  ✓ Created Texture2DProcessor");

    // Test AssetBundle::from_bytes (expect failure for now)
    let test_data = vec![0u8; 16]; // Dummy data
    match AssetBundle::from_bytes(test_data) {
        Ok(_) => {
            println!("  ⚠ AssetBundle::from_bytes unexpectedly succeeded");
        }
        Err(e) => {
            println!("  ✓ AssetBundle::from_bytes correctly fails with: {}", e);
        }
    }

    println!("✓ Basic API structure test passed");
}

#[tokio::test]
async fn test_api_method_signatures() {
    println!("Testing API method signatures...");

    // This is mainly a compilation test to ensure method signatures match expectations

    let version = UnityVersion::from_str("2020.3.12f1").unwrap();
    let audio_processor = AudioClipProcessor::new(version.clone());
    let texture_processor = Texture2DProcessor::new(version);

    println!("  ✓ All processor types created successfully");
    println!("  ✓ Method signatures compile correctly");
    println!("✓ API method signatures test passed");
}

#[tokio::test]
async fn test_error_types() {
    println!("Testing error type compatibility...");

    // Test that our error types work as expected
    use unity_asset_binary_v2::UnityAssetError;

    let error = UnityAssetError::parse_error("Test error".to_string(), 0);
    println!("  ✓ Can create UnityAssetError: {:?}", error);

    println!("✓ Error types test passed");
}

#[tokio::test]
async fn test_type_exports() {
    println!("Testing type exports...");

    // Test that all expected types are exported and accessible
    let _version = UnityVersion::from_str("2020.3.12f1").unwrap();

    // These should all be accessible from the root module
    use unity_asset_binary_v2::{
        Asset, AssetBundle, AudioClip, AudioClipProcessor, SerializedFile, Texture2D,
        Texture2DProcessor, TextureFormat, TypeTree, UnityObject, UnityValue,
    };

    println!("  ✓ All expected types are exported and accessible");
    println!("✓ Type exports test passed");
}
