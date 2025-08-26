//! Binary V2 Integration Tests
//!
//! Tests for the async binary processing functionality to ensure compatibility with the original API.

use std::collections::HashMap;
use tokio_test;
use unity_asset_binary_v2::*;

#[tokio::test]
async fn test_bundle_processor_initialization() {
    let processor = init_async_binary();
    assert!(processor.max_concurrent_bundles() > 0);

    let custom_config = BundleConfig {
        max_concurrent_bundles: 4,
        buffer_size: 32768,
        ..Default::default()
    };
    let custom_processor = init_async_binary_with_config(custom_config);
    assert_eq!(custom_processor.max_concurrent_bundles(), 4);
}

#[tokio::test]
async fn test_binary_types_functionality() {
    // Test AsyncBinaryData creation
    let data = bytes::Bytes::from_static(b"test data");
    let binary_data = AsyncBinaryData::new(data.clone(), 100);

    assert_eq!(binary_data.size, 9);
    assert_eq!(binary_data.offset, 100);
    assert!(!binary_data.needs_decompression());
    assert_eq!(binary_data.raw_data(), &data);

    // Test with compression
    let compressed_data = binary_data.with_compression(CompressionType::LZ4);
    assert!(compressed_data.needs_decompression());
    assert_eq!(
        compressed_data.compression_info(),
        Some(CompressionType::LZ4)
    );
}

#[tokio::test]
async fn test_unity_version_parsing() {
    let version_info = UnityVersionInfo::new("2022.3.5f1").unwrap();
    assert_eq!(version_info.major, 2022);
    assert_eq!(version_info.minor, 3);
    assert_eq!(version_info.patch, 5);
    assert_eq!(version_info.build, "f1");
    assert_eq!(version_info.full_version, "2022.3.5f1");

    // Test feature support
    assert!(version_info.supports_feature(UnityFeature::UnityFS));
    assert!(version_info.supports_feature(UnityFeature::LZ4Compression));
    assert!(version_info.supports_feature(UnityFeature::BrotliCompression));
}

#[tokio::test]
async fn test_compression_types() {
    // Test compression type conversion
    assert_eq!(CompressionType::from_u32(0).unwrap(), CompressionType::None);
    assert_eq!(CompressionType::from_u32(2).unwrap(), CompressionType::LZ4);
    assert_eq!(CompressionType::LZ4.as_u32(), 2);

    // Test compression type properties
    assert!(CompressionType::LZ4.is_supported());
    assert!(CompressionType::LZMA.is_supported());
    assert!(!CompressionType::LZHAM.is_supported());

    // Test invalid conversion
    assert!(CompressionType::from_u32(999).is_err());
}

#[tokio::test]
async fn test_stream_reader_functionality() {
    use std::io::Cursor;

    let data = vec![
        0x12, 0x34, 0x56, 0x78, // u32: 0x78563412 (little-endian)
        0x3f, 0x80, 0x00, 0x00, // f32: 1.0 (little-endian)
        5, 0, 0, 0, // string length
        b'h', b'e', b'l', b'l', b'o', // string content
    ];

    let cursor = Cursor::new(data);
    let mut reader = AsyncStreamReader::new(cursor);

    // Test primitive reading
    let value = reader.read_u32().await.unwrap();
    assert_eq!(value, 0x78563412);

    let float_val = reader.read_f32().await.unwrap();
    assert_eq!(float_val, 1.0);

    // Test string reading
    let string = reader.read_length_prefixed_string().await.unwrap();
    assert_eq!(string, "hello");
}

#[tokio::test]
async fn test_async_decompressor() {
    let decompressor = UnityAsyncDecompressor::new();

    // Test supported formats
    let supported = decompressor.supported_types();
    assert!(supported.contains(&CompressionType::None));
    assert!(supported.contains(&CompressionType::LZ4));
    assert!(supported.contains(&CompressionType::LZMA));
    assert!(supported.contains(&CompressionType::Brotli));

    // Test uncompressed data
    let uncompressed_data = AsyncBinaryData::new(bytes::Bytes::from_static(b"test"), 0);
    let result = decompressor.decompress(&uncompressed_data).await.unwrap();
    assert_eq!(result.as_ref(), b"test");
}

#[tokio::test]
async fn test_bundle_format_detection() {
    // Test UnityFS format detection
    let unityfs_signature = b"UnityFS\0\x00\x00\x00\x00\x00\x00\x00\x00";
    let format = BundleFormat::from_signature(unityfs_signature).unwrap();
    assert_eq!(format, BundleFormat::UnityFS);
    assert!(format.supports_compression());
    assert!(format.supports_streaming());

    // Test UnityRaw format detection
    let unityraw_signature = b"UnityRaw\x00\x00\x00\x00\x00\x00\x00\x00";
    let format = BundleFormat::from_signature(unityraw_signature).unwrap();
    assert_eq!(format, BundleFormat::UnityRaw);
    assert!(!format.supports_compression());

    // Test invalid signature
    let invalid_signature = b"Invalid\0\x00\x00\x00\x00\x00\x00\x00\x00";
    assert!(BundleFormat::from_signature(invalid_signature).is_err());
}

#[tokio::test]
async fn test_bundle_entry_type_detection() {
    assert_eq!(
        BundleEntryType::from_name("CAB-main"),
        BundleEntryType::Asset
    );
    assert_eq!(
        BundleEntryType::from_name("data.unity3d"),
        BundleEntryType::Asset
    );
    assert_eq!(
        BundleEntryType::from_name("texture.resS"),
        BundleEntryType::Resource
    );
    assert_eq!(
        BundleEntryType::from_name("metadata.json"),
        BundleEntryType::Metadata
    );
    assert_eq!(
        BundleEntryType::from_name("unknown.dat"),
        BundleEntryType::Unknown
    );
}

#[tokio::test]
async fn test_compression_info() {
    let compression_info = CompressionInfo {
        compression_type: CompressionType::LZ4,
        compressed_size: 1024,
        decompressed_size: 2048,
    };

    assert_eq!(compression_info.compression_ratio(), 0.5);
    assert_eq!(compression_info.space_savings(), 0.5);
}

#[tokio::test]
async fn test_processing_context() {
    let config = AsyncBinaryConfig::default();
    let mut context = AsyncProcessingContext::new(config);

    // Test initial state
    assert_eq!(context.position.absolute, 0);
    assert_eq!(context.stats.bytes_processed, 0);

    // Test position updates
    context.update_position(100, 1);
    assert_eq!(context.position.absolute, 100);
    assert_eq!(context.position.section_id, 1);

    // Test byte processing
    context.record_processed_bytes(50);
    assert_eq!(context.stats.bytes_processed, 50);
    assert_eq!(context.position.absolute, 150);
}

#[tokio::test]
async fn test_object_processor() {
    let processor = AsyncObjectProcessor::new();
    let stats = processor.stats().await;

    // Test initial stats
    assert_eq!(stats.objects_processed, 0);
    assert_eq!(stats.cache_hits, 0);
    assert_eq!(stats.error_count, 0);

    // Test class name mapping
    assert_eq!(AsyncObjectProcessor::get_class_name(1), "GameObject");
    assert_eq!(AsyncObjectProcessor::get_class_name(28), "Texture2D");
    assert_eq!(AsyncObjectProcessor::get_class_name(83), "AudioClip");
    assert_eq!(AsyncObjectProcessor::get_class_name(999), "UnknownClass");
}

#[cfg(feature = "texture")]
#[tokio::test]
async fn test_texture_processor() {
    use unity_asset_binary_v2::extractors::texture::*;

    let processor = AsyncTexture2DProcessor::new();

    // Test basic RGB24 to RGBA32 conversion
    let input_data = vec![255, 0, 0, 0, 255, 0]; // Red and green pixels in RGB24
    let result = processor.process_rgb24(&input_data).await.unwrap();
    assert_eq!(result, vec![255, 0, 0, 255, 0, 255, 0, 255]); // Should add alpha

    // Test texture format properties
    assert_eq!(
        UnityTextureFormat::from_id(4),
        Some(UnityTextureFormat::RGBA32)
    );
    assert!(!UnityTextureFormat::RGBA32.is_compressed());
    assert!(UnityTextureFormat::DXT1.is_compressed());
    assert_eq!(UnityTextureFormat::RGBA32.bytes_per_pixel(), Some(4));
}

#[cfg(feature = "audio")]
#[tokio::test]
async fn test_audio_processor() {
    use unity_asset_binary_v2::extractors::audio::*;

    let processor = AsyncAudioProcessor::new();

    // Test PCM processing
    let input_data = vec![0xFF, 0x7F, 0x00, 0x00]; // 16-bit samples
    let result = processor.process_pcm(&input_data, 2).await.unwrap();
    assert_eq!(result.len(), 2);
    assert!((result[0] - 1.0).abs() < 0.001); // Should be close to 1.0

    // Test audio format properties
    assert_eq!(UnityAudioFormat::from_id(1), Some(UnityAudioFormat::PCM));
    assert!(!UnityAudioFormat::PCM.is_compressed());
    assert!(UnityAudioFormat::Vorbis.is_compressed());
    assert!(UnityAudioFormat::PCM.is_lossless());
    assert_eq!(UnityAudioFormat::MP3.file_extension(), "mp3");
}

#[tokio::test]
async fn test_configuration_defaults() {
    // Test BundleConfig defaults
    let bundle_config = BundleConfig::default();
    assert_eq!(bundle_config.buffer_size, 65536);
    assert_eq!(bundle_config.max_concurrent_bundles, 8);
    assert!(bundle_config.preload_metadata);
    assert!(!bundle_config.cache_decompressed);

    // Test AssetConfig defaults
    let asset_config = AssetConfig::default();
    assert_eq!(asset_config.max_concurrent_objects, 16);
    assert_eq!(asset_config.buffer_size, 65536);
    assert!(asset_config.load_type_tree);

    // Test ReaderConfig defaults
    let reader_config = ReaderConfig::default();
    assert_eq!(reader_config.buffer_size, 65536);
    assert_eq!(reader_config.timeout_ms, 30000);
    assert!(reader_config.zero_copy);

    // Test CompressionConfig defaults
    let compression_config = CompressionConfig::default();
    assert_eq!(compression_config.buffer_size, 131072);
    assert!(compression_config.verify_checksums);
}

#[tokio::test]
async fn test_version_compatibility() {
    // Test version features
    let unity_2020 = UnityVersionInfo::new("2020.3.1f1").unwrap();
    assert!(unity_2020.supports_feature(UnityFeature::UnityFS));
    assert!(unity_2020.supports_feature(UnityFeature::LZ4Compression));
    assert!(unity_2020.supports_feature(UnityFeature::BrotliCompression));

    let unity_2018 = UnityVersionInfo::new("2018.4.36f1").unwrap();
    assert!(unity_2018.supports_feature(UnityFeature::UnityFS));
    assert!(unity_2018.supports_feature(UnityFeature::LZ4Compression));
    assert!(unity_2018.supports_feature(UnityFeature::BrotliCompression));

    let unity_old = UnityVersionInfo::new("4.6.8f1").unwrap();
    assert!(!unity_old.supports_feature(UnityFeature::UnityFS));
    assert!(!unity_old.supports_feature(UnityFeature::LZ4Compression));
    assert!(!unity_old.supports_feature(UnityFeature::BrotliCompression));
}

#[tokio::test]
async fn test_error_handling() {
    // Test invalid Unity version
    let result = UnityVersionInfo::new("invalid");
    assert!(result.is_err());

    // Test invalid compression type
    let result = CompressionType::from_u32(999);
    assert!(result.is_err());

    // Test bundle format detection with invalid signature
    let result = BundleFormat::from_signature(b"Invalid");
    assert!(result.is_err());
}

#[tokio::test]
async fn test_async_traits_compatibility() {
    // This test ensures the async traits are properly defined
    // and can be used in the expected way

    use std::io::Cursor;

    let data = vec![1, 2, 3, 4];
    let cursor = Cursor::new(data);
    let mut reader = AsyncStreamReader::new(cursor);

    // Test AsyncBinaryReader trait methods
    let bytes = reader.read_exact_bytes(2).await.unwrap();
    assert_eq!(bytes.len(), 2);

    let pos = reader.current_position().await.unwrap();
    assert_eq!(pos, 2);

    let is_end = reader.is_at_end().await.unwrap();
    assert!(!is_end);
}

#[tokio::test]
async fn test_memory_safety() {
    // Test that large allocations are properly handled
    let config = AsyncBinaryConfig {
        max_read_size: 1024, // Small max read size
        ..Default::default()
    };

    // This should not cause memory issues
    let data = AsyncBinaryData::new(bytes::Bytes::from(vec![0u8; 2048]), 0);
    assert_eq!(data.size, 2048);
}

#[tokio::test]
async fn test_concurrent_processing() {
    // Test that concurrent processing works correctly
    let processor = AsyncBundleProcessor::new();

    // This is a basic test - in practice we'd test with actual bundle files
    let stats = processor.stats().await;
    assert_eq!(stats.objects_processed, 0);

    // Test max concurrent limit
    assert_eq!(processor.max_concurrent_bundles(), 8);
}
