//! Sprite processing implementation
//!
//! This module provides high-level sprite processing functionality including
//! image extraction and sprite atlas handling.

use super::parser::SpriteParser;
use super::types::*;
use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::texture::Texture2D;
use crate::unity_version::UnityVersion;
use image::{RgbaImage, imageops};

/// Sprite processor
///
/// This struct provides high-level methods for processing Unity Sprite objects,
/// including parsing, image extraction, and atlas handling.
pub struct SpriteProcessor {
    parser: SpriteParser,
    config: SpriteConfig,
}

impl SpriteProcessor {
    /// Create a new Sprite processor
    pub fn new(version: UnityVersion) -> Self {
        Self {
            parser: SpriteParser::new(version),
            config: SpriteConfig::default(),
        }
    }

    /// Create a Sprite processor with custom configuration
    pub fn with_config(version: UnityVersion, config: SpriteConfig) -> Self {
        Self {
            parser: SpriteParser::new(version),
            config,
        }
    }

    /// Parse Sprite from Unity object
    pub fn parse_sprite(&self, object: &UnityObject) -> Result<SpriteResult> {
        self.parser.parse_from_unity_object(object)
    }

    /// Process sprite with image extraction
    pub fn process_sprite_with_texture(
        &self,
        sprite_object: &UnityObject,
        texture: &Texture2D,
    ) -> Result<SpriteResult> {
        let mut result = self.parse_sprite(sprite_object)?;

        if self.config.extract_images {
            match self.extract_sprite_image(&result.sprite, texture) {
                Ok(image_data) => {
                    result = result.with_image(image_data);
                }
                Err(e) => {
                    result.add_warning(format!("Failed to extract sprite image: {}", e));
                }
            }
        }

        Ok(result)
    }

    /// Extract sprite image from texture
    pub fn extract_sprite_image(&self, sprite: &Sprite, texture: &Texture2D) -> Result<Vec<u8>> {
        // Get texture image data using converter
        let converter = crate::texture::Texture2DConverter::new(self.parser.version().clone());
        let texture_image = converter.decode_to_image(texture)?;

        // Calculate sprite bounds
        let sprite_rect = sprite.get_rect();
        let texture_width = texture_image.width();
        let texture_height = texture_image.height();

        // Validate sprite bounds
        if sprite_rect.x < 0.0
            || sprite_rect.y < 0.0
            || sprite_rect.x + sprite_rect.width > texture_width as f32
            || sprite_rect.y + sprite_rect.height > texture_height as f32
        {
            return Err(BinaryError::invalid_data(
                "Sprite rect is outside texture bounds",
            ));
        }

        // Check size limits
        if let Some((max_width, max_height)) = self.config.max_sprite_size
            && (sprite_rect.width > max_width as f32 || sprite_rect.height > max_height as f32)
        {
            return Err(BinaryError::invalid_data(
                "Sprite size exceeds maximum allowed size",
            ));
        }

        // Extract sprite region
        let x = sprite_rect.x as u32;
        let y = sprite_rect.y as u32;
        let width = sprite_rect.width as u32;
        let height = sprite_rect.height as u32;

        // Unity uses bottom-left origin, but image crate uses top-left
        // So we need to flip the Y coordinate
        let flipped_y = texture_height - y - height;

        let sprite_image =
            imageops::crop_imm(&texture_image, x, flipped_y, width, height).to_image();

        // Apply transformations if enabled
        let final_image = if self.config.apply_transformations {
            self.apply_sprite_transformations(sprite_image, sprite)?
        } else {
            sprite_image
        };

        // Convert to PNG bytes
        let mut png_data = Vec::new();
        {
            use image::ImageEncoder;
            use image::codecs::png::PngEncoder;

            let encoder = PngEncoder::new(&mut png_data);
            encoder
                .write_image(
                    final_image.as_raw(),
                    final_image.width(),
                    final_image.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| BinaryError::generic(format!("Failed to encode PNG: {}", e)))?;
        }

        Ok(png_data)
    }

    /// Apply sprite transformations (pivot, offset, etc.)
    fn apply_sprite_transformations(&self, image: RgbaImage, sprite: &Sprite) -> Result<RgbaImage> {
        // Apply offset if needed
        if sprite.offset_x != 0.0 || sprite.offset_y != 0.0 {
            // For now, we don't apply offset transformations to the image itself
            // This would require creating a larger canvas and positioning the sprite
            // which is more complex and depends on the use case
        }

        // Apply pivot transformations if needed
        if sprite.pivot_x != 0.5 || sprite.pivot_y != 0.5 {
            // Similar to offset, pivot transformations are typically handled
            // by the rendering system rather than modifying the image data
        }

        Ok(image)
    }

    /// Process sprite atlas
    pub fn process_sprite_atlas(&self, atlas_sprites: &[&UnityObject]) -> Result<SpriteAtlas> {
        if !self.config.process_atlas {
            return Err(BinaryError::unsupported("Atlas processing is disabled"));
        }

        let mut atlas = SpriteAtlas {
            name: "SpriteAtlas".to_string(),
            ..Default::default()
        };

        for sprite_obj in atlas_sprites {
            let sprite_result = self.parse_sprite(sprite_obj)?;
            let sprite = sprite_result.sprite;

            let sprite_info = SpriteInfo {
                name: sprite.name.clone(),
                rect: sprite.get_rect(),
                offset: sprite.get_offset(),
                pivot: sprite.get_pivot(),
                border: sprite.get_border(),
                pixels_to_units: sprite.pixels_to_units,
                is_polygon: sprite.is_polygon,
                texture_path_id: sprite.render_data.texture_path_id,
                is_atlas_sprite: sprite.is_atlas_sprite(),
            };

            atlas.sprites.push(sprite_info);

            if sprite.is_atlas_sprite() {
                atlas.packed_sprites.push(sprite.name);
            }
        }

        Ok(atlas)
    }

    /// Get supported sprite features for this Unity version
    pub fn get_supported_features(&self) -> Vec<&'static str> {
        let version = self.parser.version();
        let mut features = vec!["basic_sprite", "rect", "pivot"];

        if version.major >= 5 {
            features.push("border");
            features.push("pixels_to_units");
        }

        if version.major >= 2017 {
            features.push("polygon_sprites");
            features.push("sprite_atlas");
        }

        if version.major >= 2018 {
            features.push("sprite_mesh");
            features.push("sprite_physics");
        }

        features
    }

    /// Check if a feature is supported
    pub fn is_feature_supported(&self, feature: &str) -> bool {
        self.get_supported_features().contains(&feature)
    }

    /// Get the current configuration
    pub fn config(&self) -> &SpriteConfig {
        &self.config
    }

    /// Set the configuration
    pub fn set_config(&mut self, config: SpriteConfig) {
        self.config = config;
    }

    /// Get the Unity version
    pub fn version(&self) -> &UnityVersion {
        self.parser.version()
    }

    /// Set the Unity version
    pub fn set_version(&mut self, version: UnityVersion) {
        self.parser.set_version(version);
    }

    /// Validate sprite data
    pub fn validate_sprite(&self, sprite: &Sprite) -> Result<()> {
        // Check basic validity
        if sprite.rect_width <= 0.0 || sprite.rect_height <= 0.0 {
            return Err(BinaryError::invalid_data("Sprite has invalid dimensions"));
        }

        if sprite.pixels_to_units <= 0.0 {
            return Err(BinaryError::invalid_data(
                "Sprite has invalid pixels_to_units",
            ));
        }

        // Check pivot bounds
        if sprite.pivot_x < 0.0
            || sprite.pivot_x > 1.0
            || sprite.pivot_y < 0.0
            || sprite.pivot_y > 1.0
        {
            return Err(BinaryError::invalid_data("Sprite pivot is out of bounds"));
        }

        // Check size limits if configured
        if let Some((max_width, max_height)) = self.config.max_sprite_size
            && (sprite.rect_width > max_width as f32 || sprite.rect_height > max_height as f32)
        {
            return Err(BinaryError::invalid_data(
                "Sprite size exceeds maximum allowed size",
            ));
        }

        Ok(())
    }

    /// Get sprite statistics
    pub fn get_sprite_stats(&self, sprites: &[&Sprite]) -> SpriteStats {
        let mut stats = SpriteStats {
            total_sprites: sprites.len(),
            ..Default::default()
        };

        for sprite in sprites {
            stats.total_area += sprite.get_area();

            if sprite.has_border() {
                stats.nine_slice_count += 1;
            }

            if sprite.is_polygon {
                stats.polygon_count += 1;
            }

            if sprite.is_atlas_sprite() {
                stats.atlas_sprite_count += 1;
            }

            // Track size distribution
            let area = sprite.get_area();
            if area < 1024.0 {
                stats.small_sprites += 1;
            } else if area < 16384.0 {
                stats.medium_sprites += 1;
            } else {
                stats.large_sprites += 1;
            }
        }

        if !sprites.is_empty() {
            stats.average_area = stats.total_area / sprites.len() as f32;
        }

        stats
    }
}

impl Default for SpriteProcessor {
    fn default() -> Self {
        Self::new(UnityVersion::default())
    }
}

/// Sprite processing statistics
#[derive(Debug, Clone, Default)]
pub struct SpriteStats {
    pub total_sprites: usize,
    pub total_area: f32,
    pub average_area: f32,
    pub nine_slice_count: usize,
    pub polygon_count: usize,
    pub atlas_sprite_count: usize,
    pub small_sprites: usize,  // < 32x32
    pub medium_sprites: usize, // 32x32 to 128x128
    pub large_sprites: usize,  // > 128x128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_creation() {
        let version = UnityVersion::default();
        let processor = SpriteProcessor::new(version);
        assert_eq!(processor.version(), &UnityVersion::default());
    }

    #[test]
    fn test_supported_features() {
        let version = UnityVersion::parse_version("2020.3.12f1").unwrap();
        let processor = SpriteProcessor::new(version);

        let features = processor.get_supported_features();
        assert!(features.contains(&"basic_sprite"));
        assert!(features.contains(&"polygon_sprites"));
        assert!(features.contains(&"sprite_atlas"));
        assert!(processor.is_feature_supported("sprite_mesh"));
    }

    #[test]
    fn test_sprite_validation() {
        let processor = SpriteProcessor::default();
        let mut sprite = Sprite::default();

        // Invalid sprite (zero dimensions)
        assert!(processor.validate_sprite(&sprite).is_err());

        // Valid sprite
        sprite.rect_width = 100.0;
        sprite.rect_height = 100.0;
        assert!(processor.validate_sprite(&sprite).is_ok());

        // Invalid pivot
        sprite.pivot_x = 2.0;
        assert!(processor.validate_sprite(&sprite).is_err());
    }
}
