//! UnityPy Compatibility Tests
//!
//! These tests use the same sample files as UnityPy to ensure compatibility
//! and identify areas where our implementation needs improvement.

#![allow(clippy::needless_range_loop)]
#![allow(clippy::manual_flatten)]

use std::fs;
use std::path::Path;
use unity_asset_binary::{AssetBundle, SerializedFile};

/// Test parsing the char_118_yuki.ab file from UnityPy samples
#[test]
fn test_char_118_yuki_ab() {
    let sample_path = Path::new("tests/samples/char_118_yuki.ab");

    if !sample_path.exists() {
        println!("Skipping test - sample file not found: {:?}", sample_path);
        return;
    }

    println!("Testing UnityPy sample: char_118_yuki.ab");

    // Read the file
    let data = match fs::read(sample_path) {
        Ok(data) => data,
        Err(e) => {
            println!("Failed to read sample file: {}", e);
            return;
        }
    };

    println!("File size: {} bytes", data.len());

    // Try to parse as AssetBundle
    match AssetBundle::from_bytes(data.clone()) {
        Ok(bundle) => {
            println!("✓ Successfully parsed as AssetBundle");
            println!("  Signature: {}", bundle.header.signature);
            println!("  Version: {}", bundle.header.version);
            println!("  Unity Version: {}", bundle.header.unity_version);
            println!("  Unity Revision: {}", bundle.header.unity_revision);
            println!("  Size: {} bytes", bundle.header.size);
            println!("  Compression: {:?}", bundle.header.compression_type());
            println!("  Blocks: {}", bundle.blocks.len());
            println!("  Files: {}", bundle.files.len());
            println!("  Assets: {}", bundle.assets.len());

            // List all files in the bundle
            for (i, file_name) in bundle.file_names().iter().enumerate() {
                println!("    File {}: {}", i + 1, file_name);
            }

            // Try to extract objects from assets
            for (i, asset) in bundle.assets().iter().enumerate() {
                println!("  Asset {}: {}", i + 1, asset.name());
                match asset.get_objects() {
                    Ok(objects) => {
                        println!("    Objects: {}", objects.len());
                        for (j, obj) in objects.iter().take(5).enumerate() {
                            // Show first 5 objects
                            let name = obj.name().unwrap_or_else(|| "<unnamed>".to_string());
                            println!("      Object {}: {} ({})", j + 1, name, obj.class_name());
                        }
                        if objects.len() > 5 {
                            println!("      ... and {} more objects", objects.len() - 5);
                        }
                    }
                    Err(e) => {
                        println!("    Failed to extract objects: {}", e);
                    }
                }
            }
        }
        Err(e) => {
            println!("⚠ Failed to parse as AssetBundle: {}", e);

            // Try to parse as SerializedFile
            match SerializedFile::from_bytes(data.clone()) {
                Ok(asset) => {
                    println!("✓ Successfully parsed as SerializedFile");
                    println!("  Unity Version: {}", asset.unity_version());
                    println!("  Target Platform: {}", asset.target_platform());

                    match asset.get_objects() {
                        Ok(objects) => {
                            println!("  Objects: {}", objects.len());
                            for (i, obj) in objects.iter().take(10).enumerate() {
                                let name = obj.name().unwrap_or_else(|| "<unnamed>".to_string());
                                println!("    Object {}: {} ({})", i + 1, name, obj.class_name());
                            }
                        }
                        Err(e) => {
                            println!("  Failed to extract objects: {}", e);
                        }
                    }
                }
                Err(e) => {
                    println!("✗ Failed to parse as both AssetBundle and SerializedFile");
                    println!("  AssetBundle error: {}", e);

                    // Analyze the file header to understand the format
                    analyze_file_header(&data);
                }
            }
        }
    }
}

/// Analyze file header to understand the format
fn analyze_file_header(data: &[u8]) {
    println!("\n🔍 File Header Analysis:");

    if data.len() < 32 {
        println!("  File too small for analysis");
        return;
    }

    // Try to read as string
    let mut pos = 0;
    let mut signature = String::new();
    while pos < data.len() && pos < 20 {
        let byte = data[pos];
        if byte == 0 {
            break;
        }
        if byte.is_ascii_graphic() || byte == b' ' {
            signature.push(byte as char);
        } else {
            break;
        }
        pos += 1;
    }

    println!("  Potential signature: '{}'", signature);

    // Show first 32 bytes as hex
    print!("  First 32 bytes (hex): ");
    for i in 0..32.min(data.len()) {
        print!("{:02x} ", data[i]);
        if i % 16 == 15 {
            print!("\n                        ");
        }
    }
    println!();

    // Show first 32 bytes as ASCII
    print!("  First 32 bytes (ASCII): ");
    for i in 0..32.min(data.len()) {
        let byte = data[i];
        if byte.is_ascii_graphic() {
            print!("{}", byte as char);
        } else {
            print!(".");
        }
    }
    println!();

    // Check for known signatures
    if data.starts_with(b"UnityFS") {
        println!("  ✓ Detected UnityFS format");
    } else if data.starts_with(b"UnityWeb") {
        println!("  ✓ Detected UnityWeb format (not supported yet)");
    } else if data.starts_with(b"UnityRaw") {
        println!("  ✓ Detected UnityRaw format (not supported yet)");
    } else if data.starts_with(b"UnityArchive") {
        println!("  ✓ Detected UnityArchive format (not supported yet)");
    } else {
        println!("  ⚠ Unknown format - might be SerializedFile or encrypted");
    }
}

/// Test with multiple sample files if available
#[test]
fn test_multiple_samples() {
    let samples_dir = Path::new("tests/samples");

    if !samples_dir.exists() {
        println!("Samples directory not found, skipping test");
        return;
    }

    println!("Testing all available samples:");

    let mut tested_files = 0;
    let mut successful_parses = 0;

    if let Ok(entries) = fs::read_dir(samples_dir) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_file() {
                    tested_files += 1;

                    let file_name = path.file_name().unwrap().to_string_lossy();
                    println!("\n📁 Testing: {}", file_name);

                    match fs::read(&path) {
                        Ok(data) => {
                            println!("  Size: {} bytes", data.len());

                            // Try AssetBundle first
                            match AssetBundle::from_bytes(data.clone()) {
                                Ok(bundle) => {
                                    successful_parses += 1;
                                    println!("  ✓ Parsed as AssetBundle");
                                    println!("    Signature: {}", bundle.header.signature);
                                    println!("    Version: {}", bundle.header.version);
                                    println!("    Files: {}", bundle.files.len());
                                    println!("    Assets: {}", bundle.assets.len());
                                }
                                Err(_) => {
                                    // Try SerializedFile
                                    match SerializedFile::from_bytes(data) {
                                        Ok(asset) => {
                                            successful_parses += 1;
                                            println!("  ✓ Parsed as SerializedFile");
                                            println!(
                                                "    Unity Version: {}",
                                                asset.unity_version()
                                            );
                                        }
                                        Err(e) => {
                                            println!("  ✗ Failed to parse: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("  ✗ Failed to read file: {}", e);
                        }
                    }
                }
            }
        }
    }

    println!("\n📊 Summary:");
    println!("  Tested files: {}", tested_files);
    println!("  Successful parses: {}", successful_parses);
    println!(
        "  Success rate: {:.1}%",
        if tested_files > 0 {
            (successful_parses as f64 / tested_files as f64) * 100.0
        } else {
            0.0
        }
    );
}

/// Benchmark parsing performance
#[test]
fn test_parsing_performance() {
    let sample_path = Path::new("tests/samples/char_118_yuki.ab");

    if !sample_path.exists() {
        println!("Skipping performance test - sample file not found");
        return;
    }

    let data = fs::read(sample_path).unwrap();
    println!("Performance test with {} byte file", data.len());

    // Warm up
    let _ = AssetBundle::from_bytes(data.clone());

    // Benchmark parsing
    let iterations = 10;
    let start = std::time::Instant::now();

    for _ in 0..iterations {
        let _ = AssetBundle::from_bytes(data.clone());
    }

    let duration = start.elapsed();
    let avg_time = duration / iterations;

    println!("Average parsing time: {:?}", avg_time);
    println!(
        "Throughput: {:.2} MB/s",
        (data.len() as f64 / 1024.0 / 1024.0) / avg_time.as_secs_f64()
    );
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn test_error_handling() {
        // Test with empty data
        let result = AssetBundle::from_bytes(vec![]);
        assert!(result.is_err());

        // Test with invalid signature
        let result = AssetBundle::from_bytes(b"InvalidSignature".to_vec());
        assert!(result.is_err());

        // Test with truncated data
        let result = AssetBundle::from_bytes(b"UnityFS\x00\x00\x00\x06".to_vec());
        assert!(result.is_err());
    }
}
