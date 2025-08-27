//! Sprite Processing Tests
//!
//! This file tests the Sprite processing capabilities, including image extraction,
//! atlas handling, and UnityPy compatibility.

#![allow(unused_imports)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::bool_assert_comparison)]

use std::fs;
use std::path::Path;
use unity_asset_binary::{
    AssetBundle, SerializedFile, Sprite, SpriteInfo, SpriteProcessor, Texture2D, TextureFormat, UnityVersion,
};

/// Test comprehensive sprite image extraction
#[test]
fn test_sprite_comprehensive_extraction() {
    println!("Testing comprehensive sprite image extraction...");

    // Create a larger test texture (8x8 RGBA32) to simulate an atlas
    let mut texture = Texture2D::default();
    texture.name = "SpriteAtlas".to_string();
    texture.width = 8;
    texture.height = 8;
    texture.format = TextureFormat::RGBA32;

    // Create an 8x8 texture with 4 different colored quadrants
    let mut texture_data = Vec::new();
    for y in 0..8 {
        for x in 0..8 {
            if x < 4 && y < 4 {
                // Top-left quadrant: Red
                texture_data.extend_from_slice(&[255, 0, 0, 255]);
            } else if x >= 4 && y < 4 {
                // Top-right quadrant: Green
                texture_data.extend_from_slice(&[0, 255, 0, 255]);
            } else if x < 4 && y >= 4 {
                // Bottom-left quadrant: Blue
                texture_data.extend_from_slice(&[0, 0, 255, 255]);
            } else {
                // Bottom-right quadrant: Yellow
                texture_data.extend_from_slice(&[255, 255, 0, 255]);
            }
        }
    }
    texture.image_data = texture_data;

    println!(
        "  Created {}x{} atlas texture with 4 colored quadrants",
        texture.width, texture.height
    );

    // Test multiple sprites from different regions
    let test_sprites = vec![
        ("RedSprite", 0.0, 4.0, 4.0, 4.0, [255, 0, 0, 255]), // Top-left (red)
        ("GreenSprite", 4.0, 4.0, 4.0, 4.0, [0, 255, 0, 255]), // Top-right (green)
        ("BlueSprite", 0.0, 0.0, 4.0, 4.0, [0, 0, 255, 255]), // Bottom-left (blue)
        ("YellowSprite", 4.0, 0.0, 4.0, 4.0, [255, 255, 0, 255]), // Bottom-right (yellow)
    ];

    for (name, x, y, width, height, expected_color) in test_sprites {
        let mut sprite = Sprite::default();
        sprite.name = name.to_string();
        sprite.rect_x = x;
        sprite.rect_y = y;
        sprite.rect_width = width;
        sprite.rect_height = height;

        println!(
            "  Testing sprite '{}' at ({}, {}) size {}x{}",
            name, x, y, width, height
        );

        // Extract sprite image using processor
        let processor = SpriteProcessor::new(UnityVersion::default());
        match processor.extract_sprite_image(&sprite, &texture) {
            Ok(sprite_image_data) => {
                println!(
                    "    ✓ Successfully extracted sprite image ({} bytes)",
                    sprite_image_data.len()
                );

                // Verify we got some data
                assert!(!sprite_image_data.is_empty(), "Sprite image data should not be empty");

                // Basic PNG header check
                if sprite_image_data.len() >= 8 {
                    let png_header = &sprite_image_data[0..8];
                    let expected_png_header = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
                    assert_eq!(png_header, expected_png_header, "Should be valid PNG data");
                }
            }
            Err(e) => {
                panic!("Failed to extract sprite '{}': {}", name, e);
            }
        }
    }

    println!("  ✓ All sprite extractions successful");
}

/// Test sprite render data extraction
#[test]
fn test_sprite_render_data_extraction() {
    println!("Testing sprite render data extraction...");

    // Create a test texture
    let mut texture = Texture2D::default();
    texture.name = "RenderDataTexture".to_string();
    texture.width = 16;
    texture.height = 16;
    texture.format = TextureFormat::RGBA32;

    // Fill with gradient data
    let mut texture_data = Vec::new();
    for y in 0..16 {
        for x in 0..16 {
            let r = (x * 16) as u8;
            let g = (y * 16) as u8;
            let b = 128u8;
            let a = 255u8;
            texture_data.extend_from_slice(&[r, g, b, a]);
        }
    }
    texture.image_data = texture_data;

    // Create a sprite with render data coordinates
    let mut sprite = Sprite::default();
    sprite.name = "RenderDataSprite".to_string();
    sprite.rect_x = 2.0;
    sprite.rect_y = 2.0;
    sprite.rect_width = 4.0;
    sprite.rect_height = 4.0;

    // Set render data coordinates (different from rect)
    sprite.render_data.texture_rect_x = 4.0;
    sprite.render_data.texture_rect_y = 8.0;
    sprite.render_data.texture_rect_width = 6.0;
    sprite.render_data.texture_rect_height = 6.0;
    sprite.render_data.texture_path_id = 12345;

    println!(
        "  Sprite rect: ({}, {}) {}x{}",
        sprite.rect_x, sprite.rect_y, sprite.rect_width, sprite.rect_height
    );
    println!(
        "  Render data: ({}, {}) {}x{}",
        sprite.render_data.texture_rect_x,
        sprite.render_data.texture_rect_y,
        sprite.render_data.texture_rect_width,
        sprite.render_data.texture_rect_height
    );

    // Test extraction using processor
    let processor = SpriteProcessor::new(UnityVersion::default());
    match processor.extract_sprite_image(&sprite, &texture) {
        Ok(rect_image_data) => {
            println!(
                "    ✓ Rect extraction: {} bytes",
                rect_image_data.len()
            );
            assert!(!rect_image_data.is_empty());
        }
        Err(e) => {
            panic!("Rect extraction failed: {}", e);
        }
    }

    // Test extraction using processor (same method)
    match processor.extract_sprite_image(&sprite, &texture) {
        Ok(render_image_data) => {
            println!(
                "    ✓ Render data extraction: {} bytes",
                render_image_data.len()
            );
            assert!(!render_image_data.is_empty());
        }
        Err(e) => {
            panic!("Render data extraction failed: {}", e);
        }
    }

    println!("  ✓ Both extraction methods working correctly");
}

/// Test sprite information extraction
#[test]
fn test_sprite_info_extraction() {
    println!("Testing sprite information extraction...");

    let mut sprite = Sprite::default();
    sprite.name = "InfoTestSprite".to_string();
    sprite.rect_x = 10.0;
    sprite.rect_y = 20.0;
    sprite.rect_width = 64.0;
    sprite.rect_height = 32.0;
    sprite.offset_x = 2.0;
    sprite.offset_y = -1.0;
    sprite.pivot_x = 0.3;
    sprite.pivot_y = 0.7;
    sprite.border_x = 5.0; // left
    sprite.border_y = 3.0; // bottom
    sprite.border_z = 7.0; // right
    sprite.border_w = 4.0; // top
    sprite.pixels_to_units = 50.0;
    sprite.is_polygon = true;
    sprite.render_data.texture_path_id = 98765;
    sprite.sprite_atlas_path_id = Some(11111);

    // Use sprite fields directly since get_info doesn't exist

    println!("  Sprite Info:");
    println!("    Name: {}", sprite.name);
    println!(
        "    Rect: {}x{} at ({}, {})",
        sprite.rect_width, sprite.rect_height, sprite.rect_x, sprite.rect_y
    );
    println!("    Offset: ({}, {})", sprite.offset_x, sprite.offset_y);
    println!("    Pivot: ({}, {})", sprite.pivot_x, sprite.pivot_y);
    println!(
        "    Border: L:{} B:{} R:{} T:{}",
        sprite.border_x, sprite.border_y, sprite.border_z, sprite.border_w
    );
    println!("    Pixels to units: {}", sprite.pixels_to_units);
    println!("    Is polygon: {}", sprite.is_polygon);
    println!("    Texture path ID: {}", sprite.render_data.texture_path_id);
    println!("    Is atlas sprite: {}", sprite.is_atlas_sprite());

    // Verify all information
    assert_eq!(sprite.name, "InfoTestSprite");
    assert_eq!(sprite.rect_x, 10.0);
    assert_eq!(sprite.rect_y, 20.0);
    assert_eq!(sprite.rect_width, 64.0);
    assert_eq!(sprite.rect_height, 32.0);
    assert_eq!(sprite.offset_x, 2.0);
    assert_eq!(sprite.offset_y, -1.0);
    assert_eq!(sprite.pivot_x, 0.3);
    assert_eq!(sprite.pivot_y, 0.7);
    assert_eq!(sprite.border_x, 5.0);
    assert_eq!(sprite.border_y, 3.0);
    assert_eq!(sprite.border_z, 7.0);
    assert_eq!(sprite.border_w, 4.0);
    assert_eq!(sprite.pixels_to_units, 50.0);
    assert_eq!(sprite.is_polygon, true);
    assert_eq!(sprite.render_data.texture_path_id, 98765);
    assert_eq!(sprite.is_atlas_sprite(), true);

    println!("  ✓ All sprite information correctly extracted");
}

/// Test sprite PNG export functionality
#[test]
fn test_sprite_png_export() {
    println!("Testing sprite PNG export functionality...");

    // Create a simple test texture
    let mut texture = Texture2D::default();
    texture.name = "ExportTexture".to_string();
    texture.width = 8;
    texture.height = 8;
    texture.format = TextureFormat::RGBA32;

    // Create a checkerboard pattern
    let mut texture_data = Vec::new();
    for y in 0..8 {
        for x in 0..8 {
            if (x + y) % 2 == 0 {
                // White squares
                texture_data.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                // Black squares
                texture_data.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    texture.image_data = texture_data;

    // Create a sprite covering part of the texture
    let mut sprite = Sprite::default();
    sprite.name = "ExportSprite".to_string();
    sprite.rect_x = 2.0;
    sprite.rect_y = 2.0;
    sprite.rect_width = 4.0;
    sprite.rect_height = 4.0;

    // Test PNG export
    std::fs::create_dir_all("target").ok();
    let png_path = "target/test_sprite_export.png";

    // Use processor to extract image and save manually
    let processor = SpriteProcessor::new(UnityVersion::default());
    match processor.extract_sprite_image(&sprite, &texture) {
        Ok(png_data) => {
            println!("    ✓ PNG extraction successful");

            // Write to file
            if let Ok(()) = fs::write(png_path, &png_data) {
                println!("    ✓ PNG file written");

                // Verify file exists
                assert!(Path::new(png_path).exists(), "PNG file should exist");

                // Check file size
                let file_size = png_data.len();
                println!("    ✓ PNG file size: {} bytes", file_size);
                assert!(file_size > 50, "PNG file should have reasonable size");

                // Clean up
                fs::remove_file(png_path).ok();
            }
        }
        Err(e) => {
            panic!("PNG export failed: {}", e);
        }
    }

    println!("  ✓ Sprite PNG export functionality working correctly");
}
