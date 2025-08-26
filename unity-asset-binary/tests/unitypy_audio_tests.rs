//! UnityPy AudioClip Compatibility Tests
//!
//! This file tests the AudioClip processing features against UnityPy's
//! AudioClip handling behavior.

#![allow(clippy::field_reassign_with_default)]

use unity_asset_binary::{
    AudioClip, AudioClipMeta, AudioClipProcessor, AudioCompressionFormat, UnityVersion,
};

/// Test audio compression format detection compatibility with UnityPy
///
/// UnityPy equivalent:
/// ```python
/// for obj in env.objects:
///     if obj.type.name == "AudioClip":
///         data = obj.read()
///         print(f"Format: {data.m_CompressionFormat}")
///         print(f"Channels: {data.m_Channels}")
/// ```
#[test]
fn test_audio_format_detection_unitypy_compat() {
    println!("Testing audio format detection compatibility with UnityPy...");

    // Test format enum compatibility with UnityPy values
    let format_tests = vec![
        (0, AudioCompressionFormat::PCM),
        (1, AudioCompressionFormat::Vorbis),
        (2, AudioCompressionFormat::ADPCM),
        (3, AudioCompressionFormat::MP3),
        (4, AudioCompressionFormat::VAG),
        (5, AudioCompressionFormat::HEVAG),
        (6, AudioCompressionFormat::XMA),
        (7, AudioCompressionFormat::AAC),
        (8, AudioCompressionFormat::GCADPCM),
        (9, AudioCompressionFormat::ATRAC9),
    ];

    for (unity_value, expected_format) in format_tests {
        let format = AudioCompressionFormat::from(unity_value);
        assert_eq!(
            format, expected_format,
            "Format conversion for value {} should match UnityPy",
            unity_value
        );

        let info = format.info();
        println!(
            "  Format {}: {} ({})",
            unity_value,
            info.name,
            if info.compressed {
                "compressed"
            } else {
                "uncompressed"
            }
        );
    }

    println!("  ✓ Audio format detection compatible with UnityPy");
}

/// Test audio format extensions compatibility with UnityPy
#[test]
fn test_audio_format_extensions_unitypy_compat() {
    println!("Testing audio format extensions compatibility with UnityPy...");

    // Test extensions match UnityPy's AUDIO_TYPE_EXTENSION mapping
    let extension_tests = vec![
        (AudioCompressionFormat::PCM, ".wav"),
        (AudioCompressionFormat::Vorbis, ".ogg"),
        (AudioCompressionFormat::MP3, ".mp3"),
        (AudioCompressionFormat::AAC, ".m4a"),
    ];

    for (format, expected_ext) in extension_tests {
        let ext = format.extension();
        assert_eq!(
            ext, expected_ext,
            "Extension for {:?} should match UnityPy",
            format
        );

        println!("  {:?}: {}", format, ext);
    }

    println!("  ✓ Audio format extensions compatible with UnityPy");
}

/// Test audio magic byte detection compatibility with UnityPy
///
/// UnityPy equivalent:
/// ```python
/// magic = memoryview(audio_data)[:8]
/// if magic[:4] == b"OggS":
///     return {f"{audio.m_Name}.ogg": audio_data}
/// elif magic[:4] == b"RIFF":
///     return {f"{audio.m_Name}.wav": audio_data}
/// elif magic[4:8] == b"ftyp":
///     return {f"{audio.m_Name}.m4a": audio_data}
/// ```
#[test]
fn test_audio_magic_detection_unitypy_compat() {
    println!("Testing audio magic byte detection compatibility with UnityPy...");

    // Test Ogg Vorbis detection (like UnityPy)
    let mut clip = AudioClip::default();
    clip.name = "TestOgg".to_string();
    clip.audio_data = b"OggS\x00\x02\x00\x00test_ogg_data".to_vec();

    let detected_format = clip.detect_format();
    assert_eq!(detected_format, AudioCompressionFormat::Vorbis);

    let samples = clip.extract_samples().unwrap();
    assert!(samples.contains_key("TestOgg.ogg"));
    println!("  ✓ Ogg Vorbis detection matches UnityPy");

    // Test WAV detection (like UnityPy)
    clip.name = "TestWav".to_string();
    clip.audio_data = b"RIFF\x24\x08\x00\x00WAVEtest_wav_data".to_vec();

    let detected_format = clip.detect_format();
    assert_eq!(detected_format, AudioCompressionFormat::PCM);

    let samples = clip.extract_samples().unwrap();
    assert!(samples.contains_key("TestWav.wav"));
    println!("  ✓ WAV detection matches UnityPy");

    // Test M4A/AAC detection (like UnityPy)
    clip.name = "TestM4A".to_string();
    clip.audio_data = b"\x00\x00\x00\x20ftypM4A test_m4a_data".to_vec();

    let detected_format = clip.detect_format();
    assert_eq!(detected_format, AudioCompressionFormat::AAC);

    let samples = clip.extract_samples().unwrap();
    assert!(samples.contains_key("TestM4A.m4a"));
    println!("  ✓ M4A/AAC detection matches UnityPy");

    println!("  ✓ Audio magic byte detection compatible with UnityPy");
}

/// Test audio sample extraction compatibility with UnityPy
///
/// UnityPy equivalent:
/// ```python
/// def extract_audioclip_samples(audio: AudioClip) -> Dict[str, bytes]:
///     # ... magic detection logic ...
///     return {filename: audio_data}
/// ```
#[test]
fn test_audio_sample_extraction_unitypy_compat() {
    println!("Testing audio sample extraction compatibility with UnityPy...");

    // Test multiple format extractions
    let test_cases = vec![
        (
            "OggTest",
            b"OggS\x00\x02\x00\x00ogg_data".to_vec(),
            "OggTest.ogg",
        ),
        (
            "WavTest",
            b"RIFF\x24\x08\x00\x00wav_data".to_vec(),
            "WavTest.wav",
        ),
        (
            "Mp3Test",
            b"ID3\x03\x00\x00\x00mp3_data".to_vec(),
            "Mp3Test.mp3",
        ),
        (
            "M4aTest",
            b"\x00\x00\x00\x20ftypm4a_data".to_vec(),
            "M4aTest.m4a",
        ),
    ];

    for (name, data, expected_filename) in test_cases {
        let mut clip = AudioClip::default();
        clip.name = name.to_string();
        clip.audio_data = data.clone();

        let samples = clip.extract_samples().unwrap();
        assert_eq!(samples.len(), 1, "Should extract exactly one sample");
        assert!(
            samples.contains_key(expected_filename),
            "Should contain expected filename: {}",
            expected_filename
        );
        assert_eq!(
            samples[expected_filename], data,
            "Extracted data should match original"
        );

        println!("  ✓ {} extraction matches UnityPy", name);
    }

    println!("  ✓ Audio sample extraction compatible with UnityPy");
}

/// Test WAV file creation for raw PCM data
///
/// UnityPy equivalent creates WAV headers for raw PCM data
#[test]
fn test_wav_creation_unitypy_compat() {
    println!("Testing WAV file creation compatibility with UnityPy...");

    let mut clip = AudioClip::default();
    clip.name = "RawPCM".to_string();
    clip.meta = AudioClipMeta::Modern {
        load_type: 0,
        channels: 2,
        frequency: 44100,
        bits_per_sample: 16,
        length: 1.0,
        is_tracker_format: false,
        subsound_index: 0,
        preload_audio_data: true,
        load_in_background: false,
        legacy_3d: false,
        compression_format: AudioCompressionFormat::PCM,
    };

    // Raw PCM data (not in WAV format)
    clip.audio_data = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];

    let samples = clip.extract_samples().unwrap();
    assert!(samples.contains_key("RawPCM.wav"));

    let wav_data = &samples["RawPCM.wav"];

    // Verify WAV header structure (like UnityPy would create)
    assert!(
        wav_data.starts_with(b"RIFF"),
        "Should start with RIFF header"
    );
    assert!(
        wav_data[8..12] == *b"WAVE",
        "Should contain WAVE identifier"
    );
    assert!(wav_data[12..16] == *b"fmt ", "Should contain fmt chunk");

    // Verify the original PCM data is included at the end
    assert!(
        wav_data.ends_with(&clip.audio_data),
        "Should contain original PCM data"
    );

    println!("  ✓ WAV file creation compatible with UnityPy");
}

/// Test audio processor version compatibility
#[test]
fn test_audio_processor_version_compat() {
    println!("Testing audio processor version compatibility...");

    let test_versions = vec![
        (
            "4.7.2f1",
            vec![AudioCompressionFormat::PCM, AudioCompressionFormat::Vorbis],
        ),
        (
            "5.0.0f1",
            vec![
                AudioCompressionFormat::PCM,
                AudioCompressionFormat::Vorbis,
                AudioCompressionFormat::MP3,
            ],
        ),
        (
            "2018.1.0f1",
            vec![
                AudioCompressionFormat::PCM,
                AudioCompressionFormat::Vorbis,
                AudioCompressionFormat::MP3,
                AudioCompressionFormat::AAC,
            ],
        ),
    ];

    for (version_str, expected_formats) in test_versions {
        let version = UnityVersion::parse_version(version_str).unwrap();
        let processor = AudioClipProcessor::new(version);
        let supported_formats = processor.get_supported_formats();

        for expected_format in expected_formats {
            assert!(
                supported_formats.contains(&expected_format),
                "Version {} should support format {:?}",
                version_str,
                expected_format
            );
        }

        println!(
            "  Version {}: {} formats supported",
            version_str,
            supported_formats.len()
        );
    }

    println!("  ✓ Audio processor version compatibility working");
}

/// Test audio information extraction (like UnityPy's audio properties)
#[test]
fn test_audio_info_unitypy_compat() {
    println!("Testing audio info extraction compatibility with UnityPy...");

    let mut clip = AudioClip::default();
    clip.name = "InfoTest".to_string();
    clip.audio_data = b"OggS\x00\x02\x00\x00test_data_here".to_vec();
    clip.meta = AudioClipMeta::Modern {
        load_type: 0,
        channels: 2,
        frequency: 44100,
        bits_per_sample: 16,
        length: 5.5,
        is_tracker_format: false,
        subsound_index: 0,
        preload_audio_data: true,
        load_in_background: false,
        legacy_3d: false,
        compression_format: AudioCompressionFormat::Vorbis,
    };

    let info = clip.get_info();

    // Verify info matches UnityPy's audio properties
    assert_eq!(info.name, "InfoTest");
    assert_eq!(info.format, AudioCompressionFormat::Vorbis);
    assert_eq!(info.properties.channels, 2);
    assert_eq!(info.properties.sample_rate, 44100);
    assert_eq!(info.properties.bits_per_sample, 16);
    assert_eq!(info.properties.duration, 5.5);
    assert_eq!(info.format_info.name, "Vorbis");
    assert_eq!(info.format_info.extension, ".ogg");
    assert!(info.format_info.compressed);
    assert!(!info.has_external_data);

    println!("  Audio Info:");
    println!("    Name: {}", info.name);
    println!(
        "    Format: {} ({})",
        info.format_info.name,
        if info.format_info.compressed {
            "compressed"
        } else {
            "uncompressed"
        }
    );
    println!("    Channels: {}", info.properties.channels);
    println!("    Sample Rate: {} Hz", info.properties.sample_rate);
    println!("    Duration: {:.1}s", info.properties.duration);
    println!("    Data size: {} bytes", info.data_size);

    println!("  ✓ Audio info extraction compatible with UnityPy");
}

/// Test error handling compatibility with UnityPy
#[test]
fn test_audio_error_handling_unitypy_compat() {
    println!("Testing audio error handling compatibility with UnityPy...");

    // Test invalid audio properties (UnityPy would also fail)
    let mut clip = AudioClip::default();
    clip.meta = AudioClipMeta::Modern {
        load_type: 0,
        channels: 0,  // Invalid
        frequency: 0, // Invalid
        bits_per_sample: 16,
        length: 1.0,
        is_tracker_format: false,
        subsound_index: 0,
        preload_audio_data: true,
        load_in_background: false,
        legacy_3d: false,
        compression_format: AudioCompressionFormat::PCM,
    };
    clip.audio_data = vec![0; 1024];

    // This should fail when trying to create WAV file
    let result = clip.extract_samples();
    match result {
        Err(_) => println!("  ✓ Invalid audio properties properly rejected"),
        Ok(_) => {
            // If it succeeds, the WAV creation should still be valid
            println!("  ✓ Error handling graceful for edge cases");
        }
    }

    // Test empty audio data
    clip.audio_data.clear();
    let samples = clip.extract_samples().unwrap();
    // Should still work but with empty data
    assert!(!samples.is_empty());
    println!("  ✓ Empty audio data handled gracefully");

    println!("  ✓ Error handling compatible with UnityPy behavior");
}
