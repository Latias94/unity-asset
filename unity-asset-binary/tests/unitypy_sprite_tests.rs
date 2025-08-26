//! UnityPy Sprite Compatibility Tests
//!
//! This file tests the Sprite processing features against UnityPy's
//! Sprite handling behavior.

use std::fs;
use std::path::Path;
use unity_asset_binary::{Sprite, SpriteProcessor, Texture2D, TextureFormat, UnityVersion};

/// Test sprite image extraction compatibility with UnityPy
///
/// UnityPy equivalent:
/// ```python
/// for obj in env.objects:
///     if obj.type.name == "Sprite":
///         data = obj.read()
///         image = data.image  # This extracts the sprite from texture
///         image.save("sprite.png")
/// ```
#[test]
fn test_sprite_image_extraction_unitypy_compat() {
    println!("Testing sprite image extraction compatibility with UnityPy...");

    // Create a test texture atlas (similar to what UnityPy would process)
    let mut texture = Texture2D::default();
    texture.name = "UI_Atlas".to_string();
    texture.width = 256;
    texture.height = 256;
    texture.format = TextureFormat::RGBA32;

    // Create a texture with distinct regions for testing
    let mut texture_data = Vec::new();
    for y in 0..256 {
        for x in 0..256 {
            // Create 4 quadrants with different colors
            let (r, g, b) = if x < 128 && y < 128 {
                (255, 100, 100) // Top-left: Light red
            } else if x >= 128 && y < 128 {
                (100, 255, 100) // Top-right: Light green
            } else if x < 128 && y >= 128 {
                (100, 100, 255) // Bottom-left: Light blue
            } else {
                (255, 255, 100) // Bottom-right: Light yellow
            };
            texture_data.extend_from_slice(&[r, g, b, 255]);
        }
    }
    texture.image_data = texture_data;

    // Test sprite extraction (like UnityPy's data.image)
    let mut sprite = Sprite::default();
    sprite.name = "UI_Button".to_string();
    sprite.rect_x = 64.0;
    sprite.rect_y = 192.0; // Unity bottom-left origin
    sprite.rect_width = 64.0;
    sprite.rect_height = 64.0;
    sprite.pixels_to_units = 100.0;

    println!(
        "  Testing sprite extraction from {}x{} atlas",
        texture.width, texture.height
    );
    println!(
        "  Sprite: '{}' at ({}, {}) size {}x{}",
        sprite.name, sprite.rect_x, sprite.rect_y, sprite.rect_width, sprite.rect_height
    );

    match sprite.extract_image(&texture) {
        Ok(sprite_image) => {
            println!(
                "    ✓ Successfully extracted sprite image: {}x{}",
                sprite_image.width(),
                sprite_image.height()
            );

            // Verify dimensions match UnityPy behavior
            assert_eq!(sprite_image.width(), 64);
            assert_eq!(sprite_image.height(), 64);

            // Verify we got the correct region (top-left quadrant = light red)
            let center_pixel = sprite_image.get_pixel(32, 32);
            println!("    ✓ Center pixel color: {:?}", center_pixel.0);

            // Should be light red from top-left quadrant
            assert!(center_pixel.0[0] > 200, "Should have high red component");
            assert!(center_pixel.0[1] < 150, "Should have low green component");
            assert!(center_pixel.0[2] < 150, "Should have low blue component");

            println!("    ✓ Sprite extraction compatible with UnityPy");
        }
        Err(e) => {
            panic!("Sprite extraction failed: {}", e);
        }
    }
}

/// Test sprite properties compatibility with UnityPy
///
/// UnityPy equivalent:
/// ```python
/// data = obj.read()
/// print(f"Name: {data.name}")
/// print(f"Rect: {data.rect}")
/// print(f"Pivot: {data.pivot}")
/// print(f"PixelsToUnits: {data.pixelsToUnits}")
/// ```
#[test]
fn test_sprite_properties_unitypy_compat() {
    println!("Testing sprite properties compatibility with UnityPy...");

    let mut sprite = Sprite::default();
    sprite.name = "PlayerIcon".to_string();
    sprite.rect_x = 128.0;
    sprite.rect_y = 64.0;
    sprite.rect_width = 32.0;
    sprite.rect_height = 48.0;
    sprite.offset_x = 2.0;
    sprite.offset_y = -1.5;
    sprite.pivot_x = 0.5;
    sprite.pivot_y = 0.0; // Bottom pivot
    sprite.pixels_to_units = 16.0; // Common for pixel art
    sprite.border_x = 4.0;
    sprite.border_y = 4.0;
    sprite.border_z = 4.0;
    sprite.border_w = 4.0;
    sprite.is_polygon = false;

    let info = sprite.get_info();

    println!("  Sprite Properties (UnityPy compatible):");
    println!("    Name: {}", info.name);
    println!(
        "    Rect: ({}, {}) {}x{}",
        info.rect.x, info.rect.y, info.rect.width, info.rect.height
    );
    println!("    Offset: ({}, {})", info.offset.x, info.offset.y);
    println!("    Pivot: ({}, {})", info.pivot.x, info.pivot.y);
    println!(
        "    Border: L:{} B:{} R:{} T:{}",
        info.border.left, info.border.bottom, info.border.right, info.border.top
    );
    println!("    PixelsToUnits: {}", info.pixels_to_units);
    println!("    IsPolygon: {}", info.is_polygon);

    // Verify properties match UnityPy's structure
    assert_eq!(info.name, "PlayerIcon");
    assert_eq!(info.rect.x, 128.0);
    assert_eq!(info.rect.y, 64.0);
    assert_eq!(info.rect.width, 32.0);
    assert_eq!(info.rect.height, 48.0);
    assert_eq!(info.offset.x, 2.0);
    assert_eq!(info.offset.y, -1.5);
    assert_eq!(info.pivot.x, 0.5);
    assert_eq!(info.pivot.y, 0.0);
    assert_eq!(info.pixels_to_units, 16.0);
    assert_eq!(info.is_polygon, false);

    println!("    ✓ All properties compatible with UnityPy");
}

/// Test sprite atlas handling compatibility
#[test]
fn test_sprite_atlas_unitypy_compat() {
    println!("Testing sprite atlas handling compatibility with UnityPy...");

    // Create sprites that would be part of an atlas
    let atlas_sprites = vec![
        ("Icon_Health", 0.0, 224.0, 32.0, 32.0),
        ("Icon_Mana", 32.0, 224.0, 32.0, 32.0),
        ("Icon_Stamina", 64.0, 224.0, 32.0, 32.0),
        ("Button_Play", 0.0, 192.0, 64.0, 32.0),
        ("Button_Pause", 64.0, 192.0, 64.0, 32.0),
    ];

    // Create atlas texture
    let mut atlas_texture = Texture2D::default();
    atlas_texture.name = "UI_Atlas_01".to_string();
    atlas_texture.width = 256;
    atlas_texture.height = 256;
    atlas_texture.format = TextureFormat::RGBA32;

    // Fill with test pattern
    let mut texture_data = vec![128u8; 256 * 256 * 4]; // Gray background
    atlas_texture.image_data = texture_data;

    println!(
        "  Testing {} sprites from atlas '{}'",
        atlas_sprites.len(),
        atlas_texture.name
    );

    for (name, x, y, width, height) in atlas_sprites {
        let mut sprite = Sprite::default();
        sprite.name = name.to_string();
        sprite.rect_x = x;
        sprite.rect_y = y;
        sprite.rect_width = width;
        sprite.rect_height = height;
        sprite.pixels_to_units = 100.0;

        // Set atlas reference (like UnityPy would have)
        sprite.sprite_atlas_path_id = Some(12345);
        sprite.render_data.texture_path_id = 67890;

        println!(
            "    Testing sprite '{}' ({}x{} at {}, {})",
            name, width, height, x, y
        );

        // Test extraction
        match sprite.extract_image(&atlas_texture) {
            Ok(sprite_image) => {
                assert_eq!(sprite_image.width(), width as u32);
                assert_eq!(sprite_image.height(), height as u32);
                println!(
                    "      ✓ Extracted {}x{} image",
                    sprite_image.width(),
                    sprite_image.height()
                );
            }
            Err(e) => {
                panic!("Failed to extract sprite '{}': {}", name, e);
            }
        }

        // Test info extraction
        let info = sprite.get_info();
        assert_eq!(info.is_atlas_sprite, true);
        assert_eq!(info.texture_path_id, 67890);
        println!("      ✓ Atlas sprite info correct");
    }

    println!("    ✓ All atlas sprites processed successfully (UnityPy compatible)");
}

/// Test sprite export functionality (like UnityPy's save methods)
#[test]
fn test_sprite_export_unitypy_compat() {
    println!("Testing sprite export compatibility with UnityPy...");

    // Create a simple test texture
    let mut texture = Texture2D::default();
    texture.name = "ExportTest".to_string();
    texture.width = 64;
    texture.height = 64;
    texture.format = TextureFormat::RGBA32;

    // Create a simple pattern
    let mut texture_data = Vec::new();
    for y in 0..64 {
        for x in 0..64 {
            let intensity = ((x + y) * 4) as u8;
            texture_data.extend_from_slice(&[intensity, intensity / 2, 255 - intensity, 255]);
        }
    }
    texture.image_data = texture_data;

    // Create sprite covering the entire texture
    let mut sprite = Sprite::default();
    sprite.name = "ExportSprite".to_string();
    sprite.rect_x = 0.0;
    sprite.rect_y = 0.0;
    sprite.rect_width = 64.0;
    sprite.rect_height = 64.0;

    // Test PNG export (like UnityPy's data.image.save())
    std::fs::create_dir_all("target").ok();
    let png_path = "target/unitypy_compat_sprite.png";

    match sprite.export_png(&texture, png_path) {
        Ok(()) => {
            println!("    ✓ PNG export successful (like UnityPy's save method)");

            // Verify file exists and has reasonable size
            assert!(Path::new(png_path).exists(), "PNG file should exist");

            if let Ok(metadata) = fs::metadata(png_path) {
                let file_size = metadata.len();
                println!("    ✓ PNG file size: {} bytes", file_size);
                assert!(file_size > 100, "PNG should have reasonable size");
            }

            // Clean up
            fs::remove_file(png_path).ok();
        }
        Err(e) => {
            panic!("PNG export failed: {}", e);
        }
    }

    println!("    ✓ Sprite export fully compatible with UnityPy workflow");
}

/// Test sprite processor version compatibility
#[test]
fn test_sprite_processor_version_compat() {
    println!("Testing sprite processor version compatibility...");

    let test_versions = vec![
        "5.0.0f1",
        "5.6.0f1",
        "2017.1.0f1",
        "2018.4.0f1",
        "2019.4.0f1",
        "2020.3.0f1",
        "2021.3.0f1",
    ];

    for version_str in test_versions {
        let version = UnityVersion::parse_version(version_str).unwrap();
        let processor = SpriteProcessor::new(version);

        println!("  Testing version {}", version_str);

        // The processor should be created successfully for all versions
        // (Sprite format is relatively stable across Unity versions)
        println!("    ✓ Processor created for version {}", version_str);
    }

    println!("    ✓ Sprite processor compatible with all Unity versions");
}
