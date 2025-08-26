//! Core Async API Test
//!
//! Tests that the basic async API compiles and can be used

#[tokio::test]
async fn test_async_api_compilation() {
    // This test verifies that our async API has the right structure

    use unity_asset_binary_v2::{AudioProcessor, Texture2DProcessor};

    // Test that we can create async processors
    let _audio_processor = AudioProcessor::new();
    let _texture_processor = Texture2DProcessor::new();

    println!("✓ Basic async API compilation test passed");
}
