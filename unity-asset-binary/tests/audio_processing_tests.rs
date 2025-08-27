//! Audio Processing Tests
//!
//! This file tests audio processing functionality including AudioClip parsing,
//! format detection, and audio data extraction.

#![allow(unused_imports)]
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use unity_asset_binary::object::ObjectInfo;
use unity_asset_binary::{
    AudioClip, AudioCompressionFormat, AudioProcessor, load_bundle_from_memory,
};

const SAMPLES_DIR: &str = "tests/samples";

/// Get all sample files in the samples directory
fn get_sample_files() -> Vec<std::path::PathBuf> {
    let samples_path = Path::new(SAMPLES_DIR);
    if !samples_path.exists() {
        return Vec::new();
    }

    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(samples_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_file() {
                    files.push(path);
                }
            }
        }
    }
    files
}

/// Test audio format detection and classification
#[test]
fn test_audio_format_detection() {
    println!("=== Audio Format Detection Test ===");

    // Test all supported audio formats
    let formats = [
        (AudioCompressionFormat::PCM, "PCM", "wav", false, false),
        (
            AudioCompressionFormat::Vorbis,
            "Ogg Vorbis",
            "ogg",
            true,
            true,
        ),
        (AudioCompressionFormat::ADPCM, "ADPCM", "wav", true, true),
        (AudioCompressionFormat::MP3, "MP3", "mp3", true, true),
        (AudioCompressionFormat::AAC, "AAC", "aac", true, true),
        (
            AudioCompressionFormat::VAG,
            "PlayStation VAG",
            "vag",
            true,
            true,
        ),
        (AudioCompressionFormat::XMA, "Xbox XMA", "xma", true, true),
        (
            AudioCompressionFormat::ATRAC9,
            "PlayStation ATRAC9",
            "at9",
            true,
            true,
        ),
    ];

    for (format, expected_name, expected_ext, expected_compressed, expected_lossy) in formats {
        let info = format.info();
        println!("  Testing format: {:?}", format);
        println!("    Name: {} (expected: {})", info.name, expected_name);
        println!(
            "    Extension: {} (expected: {})",
            info.extension, expected_ext
        );
        println!(
            "    Compressed: {} (expected: {})",
            info.compressed, expected_compressed
        );
        println!("    Lossy: {} (expected: {})", info.lossy, expected_lossy);

        assert_eq!(info.name, expected_name);
        assert_eq!(info.extension, expected_ext);
        assert_eq!(info.compressed, expected_compressed);
        assert_eq!(info.lossy, expected_lossy);
        assert_eq!(format.extension(), expected_ext);
        assert_eq!(format.is_compressed(), expected_compressed);
        assert_eq!(format.is_lossy(), expected_lossy);
    }

    println!("  ✓ All audio format tests passed");
}

/// Test AudioClip object creation and manipulation
#[test]
fn test_audioclip_creation() {
    println!("=== AudioClip Creation Test ===");

    // Test default AudioClip
    let default_clip = AudioClip::default();
    println!("  Default AudioClip:");
    println!("    Name: '{}'", default_clip.name);
    println!("    Data size: {} bytes", default_clip.data.len());

    assert_eq!(default_clip.name, "");
    assert_eq!(default_clip.data.len(), 0);

    // Test AudioClip with specific format
    let formats_to_test = [
        AudioCompressionFormat::PCM,
        AudioCompressionFormat::Vorbis,
        AudioCompressionFormat::MP3,
    ];

    for format in formats_to_test {
        let clip = AudioClip::new(format!("test_{:?}", format), format);
        println!("  AudioClip with {:?} format:", format);
        println!("    Name: '{}'", clip.name);

        // Check that the format is correctly set
        if let unity_asset_binary::audio::types::AudioClipMeta::Modern {
            compression_format, ..
        } = &clip.meta
        {
            assert_eq!(*compression_format, format);
            println!("    Format correctly set: {:?}", compression_format);
        }
    }

    println!("  ✓ AudioClip creation tests passed");
}

/// Test audio processing from real Unity files
#[test]
fn test_audio_processing_from_files() {
    println!("=== Audio Processing from Files Test ===");

    let sample_files = get_sample_files();
    let mut total_objects = 0;
    let mut audio_objects = 0;
    let mut processed_audio = 0;

    for file_path in sample_files {
        let file_name = file_path.file_name().unwrap_or_default().to_string_lossy();
        println!("  Processing file: {}", file_name);

        if let Ok(data) = fs::read(&file_path) {
            match load_bundle_from_memory(data) {
                Ok(bundle) => {
                    for asset in &bundle.assets {
                        for asset_object_info in &asset.objects {
                            total_objects += 1;

                            // Convert to our ObjectInfo type
                            let mut object_info = ObjectInfo::new(
                                asset_object_info.path_id,
                                asset_object_info.byte_start,
                                asset_object_info.byte_size,
                                asset_object_info.type_id,
                            );
                            object_info.data = asset_object_info.data.clone();

                            let class_name = object_info.class_name();

                            // Look for AudioClip objects (Class ID 83) or any audio-related objects
                            if object_info.class_id == 83
                                || class_name.contains("Audio")
                                || class_name == "Object"
                            {
                                // Many audio objects are classified as generic "Object"
                                audio_objects += 1;
                                println!(
                                    "    Found audio object: {} (ID:{}, PathID:{})",
                                    class_name, object_info.class_id, object_info.path_id
                                );

                                // Try to process the audio object
                                if let Ok(unity_class) = object_info.parse_object() {
                                    processed_audio += 1;

                                    // Try to get audio properties
                                    if let Some(name_value) = unity_class.get("m_Name") {
                                        if let unity_asset_core::UnityValue::String(name) =
                                            name_value
                                        {
                                            println!("      Audio name: '{}'", name);
                                        }
                                    }

                                    // Look for audio format information
                                    if let Some(format_value) =
                                        unity_class.get("m_CompressionFormat")
                                    {
                                        if let unity_asset_core::UnityValue::Integer(format_id) =
                                            format_value
                                        {
                                            let format =
                                                AudioCompressionFormat::from(*format_id as i32);
                                            println!(
                                                "      Format: {:?} ({})",
                                                format,
                                                format.info().name
                                            );
                                        }
                                    }

                                    // Look for audio data size
                                    if let Some(size_value) = unity_class.get("m_Size") {
                                        if let unity_asset_core::UnityValue::Integer(size) =
                                            size_value
                                        {
                                            println!("      Data size: {} bytes", size);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("    Failed to load bundle: {}", e);
                }
            }
        }
    }

    println!("\nAudio Processing Results:");
    println!("  Total objects: {}", total_objects);
    println!("  Audio objects found: {}", audio_objects);
    println!("  Successfully processed: {}", processed_audio);

    if audio_objects > 0 {
        let processing_rate = (processed_audio as f64 / audio_objects as f64) * 100.0;
        println!("  Processing success rate: {:.1}%", processing_rate);
        assert!(
            processing_rate >= 50.0,
            "Should process at least 50% of audio objects"
        );
    }

    println!("  ✓ Audio processing test completed");
}

/// Test audio format conversion capabilities
#[test]
fn test_audio_format_conversion() {
    println!("=== Audio Format Conversion Test ===");

    // Test MIME type detection
    let mime_tests = [
        (AudioCompressionFormat::PCM, "audio/wav"),
        (AudioCompressionFormat::ADPCM, "audio/wav"),
        (AudioCompressionFormat::Vorbis, "audio/ogg"),
        (AudioCompressionFormat::MP3, "audio/mpeg"),
        (AudioCompressionFormat::AAC, "audio/aac"),
        (AudioCompressionFormat::VAG, "application/octet-stream"),
    ];

    for (format, expected_mime) in mime_tests {
        let mime_type = format.mime_type();
        println!(
            "  Format {:?} -> MIME: {} (expected: {})",
            format, mime_type, expected_mime
        );
        assert_eq!(mime_type, expected_mime);
    }

    // Test format support detection
    let supported_formats = [
        AudioCompressionFormat::PCM,
        AudioCompressionFormat::Vorbis,
        AudioCompressionFormat::ADPCM,
        AudioCompressionFormat::MP3,
        AudioCompressionFormat::AAC,
    ];

    let unsupported_formats = [
        AudioCompressionFormat::VAG,
        AudioCompressionFormat::HEVAG,
        AudioCompressionFormat::XMA,
        AudioCompressionFormat::GCADPCM,
        AudioCompressionFormat::ATRAC9,
    ];

    for format in supported_formats {
        assert!(
            format.is_supported(),
            "Format {:?} should be supported",
            format
        );
        println!("  ✓ Format {:?} is supported", format);
    }

    for format in unsupported_formats {
        assert!(
            !format.is_supported(),
            "Format {:?} should not be supported",
            format
        );
        println!(
            "  ⚠ Format {:?} is not supported (requires specialized decoder)",
            format
        );
    }

    println!("  ✓ Audio format conversion tests passed");
}

/// Test advanced audio extraction and analysis
#[test]
fn test_advanced_audio_extraction() {
    println!("=== Advanced Audio Extraction Test ===");

    let sample_files = get_sample_files();
    let mut total_objects = 0;
    let mut potential_audio = 0;
    let mut analyzed_objects = 0;

    for file_path in sample_files {
        let file_name = file_path.file_name().unwrap_or_default().to_string_lossy();
        println!("  Analyzing file: {}", file_name);

        if let Ok(data) = fs::read(&file_path) {
            match load_bundle_from_memory(data) {
                Ok(bundle) => {
                    for asset in &bundle.assets {
                        for asset_object_info in &asset.objects {
                            total_objects += 1;

                            // Convert to our ObjectInfo type
                            let mut object_info = ObjectInfo::new(
                                asset_object_info.path_id,
                                asset_object_info.byte_start,
                                asset_object_info.byte_size,
                                asset_object_info.type_id,
                            );
                            object_info.data = asset_object_info.data.clone();

                            let class_name = object_info.class_name();

                            // Analyze all objects for potential audio content
                            if let Ok(unity_class) = object_info.parse_object() {
                                analyzed_objects += 1;

                                // Look for audio-related properties
                                let has_audio_props = unity_class.properties().keys().any(|key| {
                                    key.to_lowercase().contains("audio")
                                        || key.to_lowercase().contains("sound")
                                        || key.to_lowercase().contains("clip")
                                        || key.contains("m_CompressionFormat")
                                        || key.contains("m_Frequency")
                                        || key.contains("m_Channels")
                                        || key.contains("m_BitsPerSample")
                                });

                                if has_audio_props {
                                    potential_audio += 1;
                                    println!(
                                        "    Potential audio object: {} (ID:{}, PathID:{})",
                                        class_name, object_info.class_id, object_info.path_id
                                    );

                                    // Extract audio properties
                                    if let Some(name_value) = unity_class.get("m_Name") {
                                        if let unity_asset_core::UnityValue::String(name) =
                                            name_value
                                        {
                                            println!("      Name: '{}'", name);
                                        }
                                    }

                                    // Check for compression format
                                    if let Some(format_value) =
                                        unity_class.get("m_CompressionFormat")
                                    {
                                        if let unity_asset_core::UnityValue::Integer(format_id) =
                                            format_value
                                        {
                                            let format =
                                                AudioCompressionFormat::from(*format_id as i32);
                                            println!(
                                                "      Format: {:?} ({})",
                                                format,
                                                format.info().name
                                            );
                                        }
                                    }

                                    // Check for audio properties
                                    if let Some(freq_value) = unity_class.get("m_Frequency") {
                                        if let unity_asset_core::UnityValue::Integer(freq) =
                                            freq_value
                                        {
                                            println!("      Frequency: {} Hz", freq);
                                        }
                                    }

                                    if let Some(channels_value) = unity_class.get("m_Channels") {
                                        if let unity_asset_core::UnityValue::Integer(channels) =
                                            channels_value
                                        {
                                            println!("      Channels: {}", channels);
                                        }
                                    }

                                    if let Some(bits_value) = unity_class.get("m_BitsPerSample") {
                                        if let unity_asset_core::UnityValue::Integer(bits) =
                                            bits_value
                                        {
                                            println!("      Bits per sample: {}", bits);
                                        }
                                    }

                                    // Check for audio data size
                                    if let Some(size_value) = unity_class.get("m_Size") {
                                        if let unity_asset_core::UnityValue::Integer(size) =
                                            size_value
                                        {
                                            println!("      Data size: {} bytes", size);
                                        }
                                    }

                                    // Look for streaming info
                                    if let Some(stream_value) = unity_class.get("m_Resource") {
                                        println!("      Has streaming resource");
                                    }
                                }

                                // Also check for large binary data that might be audio
                                if object_info.data.len() > 1024 && class_name == "Object" {
                                    // Check if data looks like audio (simple heuristic)
                                    let data_preview =
                                        &object_info.data[..32.min(object_info.data.len())];
                                    let has_audio_signature = data_preview.iter().any(|&b| b != 0)
                                        && data_preview.len() >= 16;

                                    if has_audio_signature {
                                        println!(
                                            "    Large binary object (potential audio): {} bytes (ID:{}, PathID:{})",
                                            object_info.data.len(),
                                            object_info.class_id,
                                            object_info.path_id
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("    Failed to load bundle: {}", e);
                }
            }
        }
    }

    println!("\nAdvanced Audio Analysis Results:");
    println!("  Total objects: {}", total_objects);
    println!("  Analyzed objects: {}", analyzed_objects);
    println!("  Potential audio objects: {}", potential_audio);

    if analyzed_objects > 0 {
        let analysis_rate = (analyzed_objects as f64 / total_objects as f64) * 100.0;
        println!("  Analysis success rate: {:.1}%", analysis_rate);
        assert!(
            analysis_rate >= 80.0,
            "Should analyze at least 80% of objects"
        );
    }

    println!("  ✓ Advanced audio extraction test completed");
}
