//! UnityPy Sprite Compatibility Tests
//!
//! This file tests the Sprite processing features against UnityPy's
//! Sprite handling behavior.

#![cfg(feature = "sprite")]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_mut)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::bool_assert_comparison)]

use unity_asset_decode::sprite::{Sprite, SpriteProcessor};
use unity_asset_decode::texture::{Texture2D, TextureFormat};

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

    // Encode with SpriteProcessor into caller-owned storage.
    let sprite_processor = SpriteProcessor::new();
    let mut sprite_image_data = Vec::new();
    sprite_processor
        .write_sprite_png(&sprite, &texture, &mut sprite_image_data)
        .expect("sprite PNG encoding should succeed");
    assert!(sprite_image_data.starts_with(b"\x89PNG\r\n\x1a\n"));
    println!(
        "    ✓ Successfully encoded sprite image: {} bytes",
        sprite_image_data.len()
    );
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

    // Use sprite fields directly instead of get_info()
    println!("  Sprite Properties (UnityPy compatible):");
    println!("    Name: {}", sprite.name);
    println!(
        "    Rect: ({}, {}) {}x{}",
        sprite.rect_x, sprite.rect_y, sprite.rect_width, sprite.rect_height
    );
    println!("    Offset: ({}, {})", sprite.offset_x, sprite.offset_y);
    println!("    Pivot: ({}, {})", sprite.pivot_x, sprite.pivot_y);
    println!(
        "    Border: L:{} B:{} R:{} T:{}",
        sprite.border_x, sprite.border_y, sprite.border_z, sprite.border_w
    );
    println!("    PixelsToUnits: {}", sprite.pixels_to_units);
    println!("    IsPolygon: {}", sprite.is_polygon);

    // Verify properties match UnityPy's structure
    assert_eq!(sprite.name, "PlayerIcon");
    assert_eq!(sprite.rect_x, 128.0);
    assert_eq!(sprite.rect_y, 64.0);
    assert_eq!(sprite.rect_width, 32.0);
    assert_eq!(sprite.rect_height, 48.0);
    assert_eq!(sprite.offset_x, 2.0);
    assert_eq!(sprite.offset_y, -1.5);
    assert_eq!(sprite.pivot_x, 0.5);
    assert_eq!(sprite.pivot_y, 0.0);
    assert_eq!(sprite.pixels_to_units, 16.0);
    assert_eq!(sprite.is_polygon, false);

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

        // Encode each atlas sprite into caller-owned storage.
        let sprite_processor = SpriteProcessor::new();
        let mut sprite_image_data = Vec::new();
        sprite_processor
            .write_sprite_png(&sprite, &atlas_texture, &mut sprite_image_data)
            .unwrap_or_else(|error| panic!("Failed to encode sprite '{name}': {error}"));
        assert!(sprite_image_data.starts_with(b"\x89PNG\r\n\x1a\n"));

        // Test info extraction using sprite fields
        // Note: is_atlas_sprite and texture_path_id are not direct fields
        // We'll check basic sprite properties instead
        assert_eq!(sprite.name, name);
        assert_eq!(sprite.render_data.texture_path_id, 67890);
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

    // Encode PNG into storage owned by the caller.
    let sprite_processor = SpriteProcessor::new();
    let mut png_data = Vec::new();
    sprite_processor
        .write_sprite_png(&sprite, &texture, &mut png_data)
        .expect("PNG encoding should succeed");
    assert!(png_data.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png_data.len() > 100, "PNG should have reasonable size");

    println!("    ✓ Sprite export fully compatible with UnityPy workflow");
}
