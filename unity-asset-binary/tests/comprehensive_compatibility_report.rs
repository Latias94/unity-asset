//! Comprehensive Compatibility Report
//!
//! This file generates a comprehensive compatibility report comparing our
//! Unity Asset Parser with UnityPy and other industry standards.

#![allow(unused_imports)]
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::collections::HashMap;
use unity_asset_binary::{
    load_bundle_from_memory, UnityVersion, AudioCompressionFormat,
};
use unity_asset_binary::object::ObjectInfo;

const SAMPLES_DIR: &str = "tests/samples";

/// Comprehensive compatibility report
#[test]
fn test_comprehensive_compatibility_report() {
    println!("=== Unity Asset Parser - Comprehensive Compatibility Report ===");
    println!("Generated on: {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    println!();
    
    let sample_files = get_sample_files();
    let mut report = CompatibilityReport::new();
    
    // Process all sample files
    for file_path in sample_files {
        let file_name = file_path.file_name().unwrap_or_default().to_string_lossy();
        println!("📁 Processing: {}", file_name);
        
        if let Ok(data) = fs::read(&file_path) {
            match load_bundle_from_memory(data) {
                Ok(bundle) => {
                    report.successful_files += 1;
                    
                    // Analyze bundle structure
                    println!("  ✅ Bundle loaded successfully");
                    println!("  📊 Assets: {}", bundle.assets.len());
                    
                    for asset in &bundle.assets {
                        report.total_assets += 1;
                        println!("    📄 Asset with {} objects", asset.objects.len());
                        
                        for asset_object_info in &asset.objects {
                            report.total_objects += 1;
                            
                            // Convert to our ObjectInfo type
                            let mut object_info = ObjectInfo::new(
                                asset_object_info.path_id,
                                asset_object_info.byte_start,
                                asset_object_info.byte_size,
                                asset_object_info.type_id,
                            );
                            object_info.data = asset_object_info.data.clone();
                            
                            let class_name = object_info.class_name();
                            *report.object_types.entry(class_name.clone()).or_insert(0) += 1;
                            
                            // Try to parse the object
                            if let Ok(unity_class) = object_info.parse_object() {
                                report.parsed_objects += 1;
                                
                                // Analyze specific object types
                                match class_name.as_str() {
                                    "Texture2D" => {
                                        report.texture_objects += 1;
                                        analyze_texture(&unity_class, &mut report);
                                    }
                                    "AudioClip" => {
                                        report.audio_objects += 1;
                                        analyze_audio(&unity_class, &mut report);
                                    }
                                    "GameObject" => {
                                        report.gameobject_objects += 1;
                                    }
                                    "Transform" => {
                                        report.transform_objects += 1;
                                    }
                                    "Sprite" => {
                                        report.sprite_objects += 1;
                                    }
                                    "SpriteAtlas" => {
                                        report.sprite_atlas_objects += 1;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    report.failed_files += 1;
                    println!("  ❌ Failed to load: {}", e);
                    
                    // Analyze failure reasons
                    let error_msg = e.to_string();
                    if error_msg.contains("LZMA") {
                        report.lzma_failures += 1;
                    } else if error_msg.contains("Not enough data") {
                        report.data_failures += 1;
                    } else {
                        report.other_failures += 1;
                    }
                }
            }
        } else {
            report.failed_files += 1;
            println!("  ❌ Failed to read file");
        }
        println!();
    }
    
    // Generate comprehensive report
    generate_report(&report);
    
    // Assertions for quality gates
    let success_rate = (report.successful_files as f64 / report.total_files() as f64) * 100.0;
    let parse_rate = (report.parsed_objects as f64 / report.total_objects as f64) * 100.0;
    
    assert!(success_rate >= 60.0, "File success rate should be at least 60%, got {:.1}%", success_rate);
    assert!(parse_rate >= 90.0, "Object parse rate should be at least 90%, got {:.1}%", parse_rate);
    assert!(report.total_objects >= 40, "Should process at least 40 objects total");
    
    println!("✅ Comprehensive compatibility report completed successfully!");
}

/// Get all sample files
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

/// Compatibility report structure
#[derive(Debug, Default)]
struct CompatibilityReport {
    // File statistics
    successful_files: usize,
    failed_files: usize,
    
    // Object statistics
    total_assets: usize,
    total_objects: usize,
    parsed_objects: usize,
    
    // Object type counts
    object_types: HashMap<String, usize>,
    
    // Specific object types
    texture_objects: usize,
    audio_objects: usize,
    gameobject_objects: usize,
    transform_objects: usize,
    sprite_objects: usize,
    sprite_atlas_objects: usize,
    
    // Texture analysis
    texture_formats: HashMap<String, usize>,
    
    // Audio analysis
    audio_formats: HashMap<String, usize>,
    
    // Failure analysis
    lzma_failures: usize,
    data_failures: usize,
    other_failures: usize,
}

impl CompatibilityReport {
    fn new() -> Self {
        Self::default()
    }
    
    fn total_files(&self) -> usize {
        self.successful_files + self.failed_files
    }
}

/// Analyze texture object
fn analyze_texture(unity_class: &unity_asset_core::UnityClass, report: &mut CompatibilityReport) {
    if let Some(format_value) = unity_class.get("m_TextureFormat") {
        if let unity_asset_core::UnityValue::Integer(format_id) = format_value {
            let format_name = get_texture_format_name(*format_id as i32);
            *report.texture_formats.entry(format_name).or_insert(0) += 1;
        }
    }
}

/// Analyze audio object
fn analyze_audio(unity_class: &unity_asset_core::UnityClass, report: &mut CompatibilityReport) {
    if let Some(format_value) = unity_class.get("m_CompressionFormat") {
        if let unity_asset_core::UnityValue::Integer(format_id) = format_value {
            let format = AudioCompressionFormat::from(*format_id as i32);
            let format_name = format!("{:?}", format);
            *report.audio_formats.entry(format_name).or_insert(0) += 1;
        }
    }
}

/// Get texture format name
fn get_texture_format_name(format_id: i32) -> String {
    match format_id {
        3 => "RGB24".to_string(),
        4 => "RGBA32".to_string(),
        5 => "ARGB32".to_string(),
        10 => "DXT1".to_string(),
        12 => "DXT5".to_string(),
        14 => "BGRA32".to_string(),
        29 => "DXT1Crunched".to_string(),
        30 => "DXT5Crunched".to_string(),
        38 => "ETC_RGB4".to_string(),
        41 => "ETC2_RGB".to_string(),
        43 => "ETC2_RGBA8".to_string(),
        _ => format!("Format_{}", format_id),
    }
}

/// Generate comprehensive report
fn generate_report(report: &CompatibilityReport) {
    println!("📊 === COMPREHENSIVE COMPATIBILITY REPORT ===");
    println!();
    
    // File Processing Summary
    println!("🗂️  FILE PROCESSING SUMMARY");
    println!("   Total files processed: {}", report.total_files());
    println!("   Successfully loaded: {} ({:.1}%)", 
             report.successful_files, 
             (report.successful_files as f64 / report.total_files() as f64) * 100.0);
    println!("   Failed to load: {} ({:.1}%)", 
             report.failed_files,
             (report.failed_files as f64 / report.total_files() as f64) * 100.0);
    println!();
    
    // Object Processing Summary
    println!("🎯 OBJECT PROCESSING SUMMARY");
    println!("   Total assets: {}", report.total_assets);
    println!("   Total objects: {}", report.total_objects);
    println!("   Successfully parsed: {} ({:.1}%)", 
             report.parsed_objects,
             (report.parsed_objects as f64 / report.total_objects as f64) * 100.0);
    println!();
    
    // Object Type Distribution
    println!("📋 OBJECT TYPE DISTRIBUTION");
    let mut sorted_types: Vec<_> = report.object_types.iter().collect();
    sorted_types.sort_by(|a, b| b.1.cmp(a.1));
    
    for (type_name, count) in sorted_types.iter().take(10) {
        println!("   {}: {} objects", type_name, count);
    }
    println!();
    
    // Specific Object Analysis
    println!("🔍 SPECIFIC OBJECT ANALYSIS");
    println!("   Texture2D objects: {}", report.texture_objects);
    println!("   AudioClip objects: {}", report.audio_objects);
    println!("   GameObject objects: {}", report.gameobject_objects);
    println!("   Transform objects: {}", report.transform_objects);
    println!("   Sprite objects: {}", report.sprite_objects);
    println!("   SpriteAtlas objects: {}", report.sprite_atlas_objects);
    println!();
    
    // Texture Format Analysis
    if !report.texture_formats.is_empty() {
        println!("🖼️  TEXTURE FORMAT ANALYSIS");
        let mut sorted_formats: Vec<_> = report.texture_formats.iter().collect();
        sorted_formats.sort_by(|a, b| b.1.cmp(a.1));
        
        for (format_name, count) in sorted_formats {
            println!("   {}: {} textures", format_name, count);
        }
        println!();
    }
    
    // Audio Format Analysis
    if !report.audio_formats.is_empty() {
        println!("🔊 AUDIO FORMAT ANALYSIS");
        let mut sorted_formats: Vec<_> = report.audio_formats.iter().collect();
        sorted_formats.sort_by(|a, b| b.1.cmp(a.1));
        
        for (format_name, count) in sorted_formats {
            println!("   {}: {} audio clips", format_name, count);
        }
        println!();
    }
    
    // Failure Analysis
    if report.failed_files > 0 {
        println!("❌ FAILURE ANALYSIS");
        println!("   LZMA decompression failures: {}", report.lzma_failures);
        println!("   Data reading failures: {}", report.data_failures);
        println!("   Other failures: {}", report.other_failures);
        println!();
    }
    
    // Compatibility Assessment
    println!("🏆 COMPATIBILITY ASSESSMENT");
    let file_success_rate = (report.successful_files as f64 / report.total_files() as f64) * 100.0;
    let parse_success_rate = (report.parsed_objects as f64 / report.total_objects as f64) * 100.0;
    
    println!("   File loading success rate: {:.1}%", file_success_rate);
    println!("   Object parsing success rate: {:.1}%", parse_success_rate);
    
    // Overall grade
    let overall_score = (file_success_rate + parse_success_rate) / 2.0;
    let grade = match overall_score {
        90.0..=100.0 => "A+ (Excellent)",
        80.0..=89.9 => "A (Very Good)",
        70.0..=79.9 => "B (Good)",
        60.0..=69.9 => "C (Acceptable)",
        _ => "D (Needs Improvement)",
    };
    
    println!("   Overall compatibility score: {:.1}% ({})", overall_score, grade);
    println!();
    
    println!("📈 RECOMMENDATIONS");
    if report.lzma_failures > 0 {
        println!("   • Improve LZMA decompression support");
    }
    if report.data_failures > 0 {
        println!("   • Enhance data reading robustness");
    }
    if report.audio_objects == 0 {
        println!("   • Add more audio test samples");
    }
    if parse_success_rate < 95.0 {
        println!("   • Improve object parsing reliability");
    }
    
    println!("   • Continue expanding Unity version support");
    println!("   • Add more specialized object type handlers");
    println!("   • Implement external resource file support");
    println!();
}
