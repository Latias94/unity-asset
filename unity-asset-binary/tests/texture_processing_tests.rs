//! Texture Processing Tests
//!
//! This file tests texture processing functionality including Texture2D parsing,
//! format detection, and texture data extraction.

#![allow(unused_imports)]
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use unity_asset_binary::{
    load_bundle_from_memory, TextureProcessor,
};
use unity_asset_binary::object::ObjectInfo;

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

/// Test texture format detection and analysis
#[test]
fn test_texture_format_detection() {
    println!("=== Texture Format Detection Test ===");
    
    let sample_files = get_sample_files();
    let mut total_objects = 0;
    let mut texture_objects = 0;
    let mut processed_textures = 0;
    let mut texture_formats = std::collections::HashMap::new();
    
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
                            
                            // Look for Texture2D objects (Class ID 28) or texture-related objects
                            if object_info.class_id == 28 || class_name == "Texture2D" || 
                               class_name.contains("Texture") {
                                texture_objects += 1;
                                println!("    Found texture object: {} (ID:{}, PathID:{})", 
                                        class_name, object_info.class_id, object_info.path_id);
                                
                                // Try to process the texture object
                                if let Ok(unity_class) = object_info.parse_object() {
                                    processed_textures += 1;
                                    
                                    // Extract texture properties
                                    if let Some(name_value) = unity_class.get("m_Name") {
                                        if let unity_asset_core::UnityValue::String(name) = name_value {
                                            println!("      Texture name: '{}'", name);
                                        }
                                    }
                                    
                                    // Check for texture format
                                    if let Some(format_value) = unity_class.get("m_TextureFormat") {
                                        if let unity_asset_core::UnityValue::Integer(format_id) = format_value {
                                            let format_name = get_texture_format_name(*format_id as i32);
                                            println!("      Format: {} (ID: {})", format_name, format_id);
                                            *texture_formats.entry(format_name).or_insert(0) += 1;
                                        }
                                    }
                                    
                                    // Check for texture dimensions
                                    if let Some(width_value) = unity_class.get("m_Width") {
                                        if let unity_asset_core::UnityValue::Integer(width) = width_value {
                                            if let Some(height_value) = unity_class.get("m_Height") {
                                                if let unity_asset_core::UnityValue::Integer(height) = height_value {
                                                    println!("      Dimensions: {}x{}", width, height);
                                                }
                                            }
                                        }
                                    }
                                    
                                    // Check for mipmap info
                                    if let Some(mipmap_value) = unity_class.get("m_MipCount") {
                                        if let unity_asset_core::UnityValue::Integer(mip_count) = mipmap_value {
                                            println!("      Mip levels: {}", mip_count);
                                        }
                                    }
                                    
                                    // Check for texture data size
                                    if let Some(data_size_value) = unity_class.get("m_DataLength") {
                                        if let unity_asset_core::UnityValue::Integer(data_size) = data_size_value {
                                            println!("      Data size: {} bytes", data_size);
                                        }
                                    }
                                    
                                    // Check for streaming info
                                    if let Some(stream_value) = unity_class.get("m_StreamData") {
                                        println!("      Has streaming data");
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
    
    println!("\nTexture Processing Results:");
    println!("  Total objects: {}", total_objects);
    println!("  Texture objects found: {}", texture_objects);
    println!("  Successfully processed: {}", processed_textures);
    
    if texture_objects > 0 {
        let processing_rate = (processed_textures as f64 / texture_objects as f64) * 100.0;
        println!("  Processing success rate: {:.1}%", processing_rate);
        assert!(processing_rate >= 80.0, "Should process at least 80% of texture objects");
    }
    
    if !texture_formats.is_empty() {
        println!("\nTexture Format Distribution:");
        let mut sorted_formats: Vec<_> = texture_formats.iter().collect();
        sorted_formats.sort_by(|a, b| b.1.cmp(a.1));
        
        for (format_name, count) in sorted_formats {
            println!("  {}: {} textures", format_name, count);
        }
    }
    
    println!("  ✓ Texture format detection test completed");
}

/// Get texture format name from format ID
fn get_texture_format_name(format_id: i32) -> String {
    match format_id {
        1 => "Alpha8".to_string(),
        2 => "ARGB4444".to_string(),
        3 => "RGB24".to_string(),
        4 => "RGBA32".to_string(),
        5 => "ARGB32".to_string(),
        7 => "RGB565".to_string(),
        10 => "DXT1".to_string(),
        12 => "DXT5".to_string(),
        13 => "RGBA4444".to_string(),
        14 => "BGRA32".to_string(),
        15 => "RHalf".to_string(),
        16 => "RGHalf".to_string(),
        17 => "RGBAHalf".to_string(),
        18 => "RFloat".to_string(),
        19 => "RGFloat".to_string(),
        20 => "RGBAFloat".to_string(),
        22 => "YUY2".to_string(),
        25 => "BC4".to_string(),
        26 => "BC5".to_string(),
        27 => "BC6H".to_string(),
        28 => "BC7".to_string(),
        29 => "DXT1Crunched".to_string(),
        30 => "DXT5Crunched".to_string(),
        34 => "PVRTC_RGB2".to_string(),
        35 => "PVRTC_RGBA2".to_string(),
        36 => "PVRTC_RGB4".to_string(),
        37 => "PVRTC_RGBA4".to_string(),
        38 => "ETC_RGB4".to_string(),
        41 => "ETC2_RGB".to_string(),
        42 => "ETC2_RGBA1".to_string(),
        43 => "ETC2_RGBA8".to_string(),
        44 => "ASTC_RGB_4x4".to_string(),
        45 => "ASTC_RGB_5x5".to_string(),
        46 => "ASTC_RGB_6x6".to_string(),
        47 => "ASTC_RGB_8x8".to_string(),
        48 => "ASTC_RGB_10x10".to_string(),
        49 => "ASTC_RGB_12x12".to_string(),
        50 => "ASTC_RGBA_4x4".to_string(),
        51 => "ASTC_RGBA_5x5".to_string(),
        52 => "ASTC_RGBA_6x6".to_string(),
        53 => "ASTC_RGBA_8x8".to_string(),
        54 => "ASTC_RGBA_10x10".to_string(),
        55 => "ASTC_RGBA_12x12".to_string(),
        56 => "ETC_RGB4_3DS".to_string(),
        57 => "ETC_RGBA8_3DS".to_string(),
        58 => "RG16".to_string(),
        59 => "R8".to_string(),
        60 => "ETC_RGB4Crunched".to_string(),
        61 => "ETC2_RGBA8Crunched".to_string(),
        _ => format!("Unknown_{}", format_id),
    }
}

/// Test texture data extraction and analysis
#[test]
fn test_texture_data_extraction() {
    println!("=== Texture Data Extraction Test ===");
    
    let sample_files = get_sample_files();
    let mut total_textures = 0;
    let mut textures_with_data = 0;
    let mut total_texture_data_size = 0u64;
    
    for file_path in sample_files {
        let file_name = file_path.file_name().unwrap_or_default().to_string_lossy();
        println!("  Analyzing file: {}", file_name);
        
        if let Ok(data) = fs::read(&file_path) {
            match load_bundle_from_memory(data) {
                Ok(bundle) => {
                    for asset in &bundle.assets {
                        for asset_object_info in &asset.objects {
                            let mut object_info = ObjectInfo::new(
                                asset_object_info.path_id,
                                asset_object_info.byte_start,
                                asset_object_info.byte_size,
                                asset_object_info.type_id,
                            );
                            object_info.data = asset_object_info.data.clone();
                            
                            let class_name = object_info.class_name();
                            
                            if class_name == "Texture2D" {
                                total_textures += 1;
                                
                                if let Ok(unity_class) = object_info.parse_object() {
                                    // Check if texture has embedded data
                                    let has_data = unity_class.get("image data").is_some() ||
                                                  unity_class.get("m_ImageData").is_some() ||
                                                  object_info.data.len() > 1024;
                                    
                                    if has_data {
                                        textures_with_data += 1;
                                        total_texture_data_size += object_info.data.len() as u64;
                                        
                                        if let Some(name_value) = unity_class.get("m_Name") {
                                            if let unity_asset_core::UnityValue::String(name) = name_value {
                                                println!("    Texture '{}' has {} bytes of data", 
                                                        name, object_info.data.len());
                                            }
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
    
    println!("\nTexture Data Analysis Results:");
    println!("  Total textures: {}", total_textures);
    println!("  Textures with data: {}", textures_with_data);
    println!("  Total texture data: {} bytes ({:.2} MB)", 
             total_texture_data_size, total_texture_data_size as f64 / (1024.0 * 1024.0));
    
    if total_textures > 0 {
        let data_rate = (textures_with_data as f64 / total_textures as f64) * 100.0;
        println!("  Textures with data: {:.1}%", data_rate);
    }
    
    println!("  ✓ Texture data extraction test completed");
}
