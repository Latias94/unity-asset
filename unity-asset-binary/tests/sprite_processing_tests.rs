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
    AssetBundle, SerializedFile, Sprite, SpriteInfo, Texture2D, TextureFormat,
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

        // Extract sprite image
        match sprite.extract_image(&texture) {
            Ok(sprite_image) => {
                println!(
                    "    ✓ Successfully extracted {}x{} image",
                    sprite_image.width(),
                    sprite_image.height()
                );

                // Verify dimensions
                assert_eq!(sprite_image.width(), width as u32);
                assert_eq!(sprite_image.height(), height as u32);

                // Verify color (check center pixel)
                let center_x = sprite_image.width() / 2;
                let center_y = sprite_image.height() / 2;
                let pixel = sprite_image.get_pixel(center_x, center_y);

                println!(
                    "    ✓ Center pixel: {:?}, expected: {:?}",
                    pixel.0, expected_color
                );
                assert_eq!(
                    pixel.0, expected_color,
                    "Sprite '{}' should have correct color",
                    name
                );
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

    // Test extraction using rect coordinates
    match sprite.extract_image(&texture) {
        Ok(rect_image) => {
            println!(
                "    ✓ Rect extraction: {}x{}",
                rect_image.width(),
                rect_image.height()
            );
            assert_eq!(rect_image.width(), 4);
            assert_eq!(rect_image.height(), 4);
        }
        Err(e) => {
            panic!("Rect extraction failed: {}", e);
        }
    }

    // Test extraction using render data coordinates
    match sprite.extract_image_from_render_data(&texture) {
        Ok(render_image) => {
            println!(
                "    ✓ Render data extraction: {}x{}",
                render_image.width(),
                render_image.height()
            );
            assert_eq!(render_image.width(), 6);
            assert_eq!(render_image.height(), 6);
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

    let info = sprite.get_info();

    println!("  Sprite Info:");
    println!("    Name: {}", info.name);
    println!(
        "    Rect: {}x{} at ({}, {})",
        info.rect.width, info.rect.height, info.rect.x, info.rect.y
    );
    println!("    Offset: ({}, {})", info.offset.x, info.offset.y);
    println!("    Pivot: ({}, {})", info.pivot.x, info.pivot.y);
    println!(
        "    Border: L:{} B:{} R:{} T:{}",
        info.border.left, info.border.bottom, info.border.right, info.border.top
    );
    println!("    Pixels to units: {}", info.pixels_to_units);
    println!("    Is polygon: {}", info.is_polygon);
    println!("    Texture path ID: {}", info.texture_path_id);
    println!("    Is atlas sprite: {}", info.is_atlas_sprite);

    // Verify all information
    assert_eq!(info.name, "InfoTestSprite");
    assert_eq!(info.rect.x, 10.0);
    assert_eq!(info.rect.y, 20.0);
    assert_eq!(info.rect.width, 64.0);
    assert_eq!(info.rect.height, 32.0);
    assert_eq!(info.offset.x, 2.0);
    assert_eq!(info.offset.y, -1.0);
    assert_eq!(info.pivot.x, 0.3);
    assert_eq!(info.pivot.y, 0.7);
    assert_eq!(info.border.left, 5.0);
    assert_eq!(info.border.bottom, 3.0);
    assert_eq!(info.border.right, 7.0);
    assert_eq!(info.border.top, 4.0);
    assert_eq!(info.pixels_to_units, 50.0);
    assert_eq!(info.is_polygon, true);
    assert_eq!(info.texture_path_id, 98765);
    assert_eq!(info.is_atlas_sprite, true);

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

    match sprite.export_png(&texture, png_path) {
        Ok(()) => {
            println!("    ✓ PNG export successful");

            // Verify file exists
            assert!(Path::new(png_path).exists(), "PNG file should exist");

            // Check file size (should be reasonable for a 4x4 PNG)
            if let Ok(metadata) = fs::metadata(png_path) {
                let file_size = metadata.len();
                println!("    ✓ PNG file size: {} bytes", file_size);
                assert!(file_size > 50, "PNG file should have reasonable size");
                assert!(
                    file_size < 1000,
                    "PNG file should not be too large for 4x4 image"
                );
            }

            // Clean up
            fs::remove_file(png_path).ok();
        }
        Err(e) => {
            panic!("PNG export failed: {}", e);
        }
    }

    println!("  ✓ Sprite PNG export functionality working correctly");
}
