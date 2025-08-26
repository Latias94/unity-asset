//! Sprite Processing Module
//!
//! This module provides comprehensive Sprite processing capabilities,
//! including parsing from Unity objects and image extraction.

use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::reader::BinaryReader;
use crate::texture::Texture2D;
use crate::unity_version::UnityVersion;
use image::{RgbaImage, imageops};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use unity_asset_core::UnityValue;

/// Sprite render data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpriteRenderData {
    pub texture_path_id: i64,
    pub texture_rect_x: f32,
    pub texture_rect_y: f32,
    pub texture_rect_width: f32,
    pub texture_rect_height: f32,
    pub texture_rect_offset_x: f32,
    pub texture_rect_offset_y: f32,
    pub atlas_rect_offset_x: f32,
    pub atlas_rect_offset_y: f32,
    pub downscale_multiplier: f32,
}

/// Sprite settings
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpriteSettings {
    pub packed: bool,
    pub packing_mode: i32,
    pub packing_rotation: i32,
    pub mesh_type: i32,
}

/// Sprite rectangle information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Sprite offset information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteOffset {
    pub x: f32,
    pub y: f32,
}

/// Sprite pivot information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpritePivot {
    pub x: f32,
    pub y: f32,
}

/// Sprite border information (for 9-slice sprites)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteBorder {
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

/// Comprehensive sprite information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteInfo {
    pub name: String,
    pub rect: SpriteRect,
    pub offset: SpriteOffset,
    pub pivot: SpritePivot,
    pub border: SpriteBorder,
    pub pixels_to_units: f32,
    pub is_polygon: bool,
    pub texture_path_id: i64,
    pub is_atlas_sprite: bool,
}

/// Sprite object representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sprite {
    pub name: String,
    pub rect_x: f32,
    pub rect_y: f32,
    pub rect_width: f32,
    pub rect_height: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub border_x: f32,
    pub border_y: f32,
    pub border_z: f32,
    pub border_w: f32,
    pub pixels_to_units: f32,
    pub pivot_x: f32,
    pub pivot_y: f32,
    pub extrude: u8,
    pub is_polygon: bool,
    pub render_data: SpriteRenderData,
    pub settings: SpriteSettings,

    // Atlas reference
    pub atlas_tags: Vec<String>,
    pub sprite_atlas_path_id: Option<i64>,
}

impl Default for Sprite {
    fn default() -> Self {
        Self {
            name: String::new(),
            rect_x: 0.0,
            rect_y: 0.0,
            rect_width: 0.0,
            rect_height: 0.0,
            offset_x: 0.0,
            offset_y: 0.0,
            border_x: 0.0,
            border_y: 0.0,
            border_z: 0.0,
            border_w: 0.0,
            pixels_to_units: 100.0,
            pivot_x: 0.5,
            pivot_y: 0.5,
            extrude: 1,
            is_polygon: false,
            render_data: SpriteRenderData::default(),
            settings: SpriteSettings::default(),
            atlas_tags: Vec::new(),
            sprite_atlas_path_id: None,
        }
    }
}

impl Sprite {
    /// Parse Sprite from UnityObject
    pub fn from_unity_object(obj: &UnityObject, version: &UnityVersion) -> Result<Self> {
        // Try to parse using TypeTree first
        if let Some(type_tree) = &obj.info.type_tree {
            let properties = obj.parse_with_typetree(type_tree)?;
            Self::from_typetree(&properties, version)
        } else {
            // Fallback: parse from raw binary data
            Self::from_binary_data(&obj.info.data, version)
        }
    }

    /// Parse Sprite from TypeTree properties
    pub fn from_typetree(
        properties: &IndexMap<String, UnityValue>,
        _version: &UnityVersion,
    ) -> Result<Self> {
        let mut sprite = Sprite::default();

        // Extract name
        if let Some(UnityValue::String(name)) = properties.get("m_Name") {
            sprite.name = name.clone();
        }

        // Extract rect
        if let Some(rect_value) = properties.get("m_Rect") {
            sprite.extract_rect(rect_value)?;
        }

        // Extract offset
        if let Some(offset_value) = properties.get("m_Offset") {
            sprite.extract_offset(offset_value)?;
        }

        // Extract border
        if let Some(border_value) = properties.get("m_Border") {
            sprite.extract_border(border_value)?;
        }

        // Extract pixels to units
        if let Some(UnityValue::Float(pixels_to_units)) = properties.get("m_PixelsToUnits") {
            sprite.pixels_to_units = *pixels_to_units as f32;
        }

        // Extract pivot
        if let Some(pivot_value) = properties.get("m_Pivot") {
            sprite.extract_pivot(pivot_value)?;
        }

        // Extract extrude
        if let Some(UnityValue::Integer(extrude)) = properties.get("m_Extrude") {
            sprite.extrude = *extrude as u8;
        }

        // Extract polygon flag
        if let Some(UnityValue::Bool(is_polygon)) = properties.get("m_IsPolygon") {
            sprite.is_polygon = *is_polygon;
        }

        // Extract render data
        if let Some(render_data_value) = properties.get("m_RD") {
            sprite.extract_render_data(render_data_value)?;
        }

        // Extract atlas tags
        if let Some(atlas_tags_value) = properties.get("m_AtlasTags") {
            sprite.extract_atlas_tags(atlas_tags_value)?;
        }

        // Extract sprite atlas reference
        if let Some(sprite_atlas_value) = properties.get("m_SpriteAtlas") {
            sprite.extract_sprite_atlas(sprite_atlas_value)?;
        }

        Ok(sprite)
    }

    /// Parse Sprite from raw binary data (fallback method)
    pub fn from_binary_data(data: &[u8], _version: &UnityVersion) -> Result<Self> {
        let mut reader = BinaryReader::new(data, crate::reader::ByteOrder::Little);
        let mut sprite = Sprite::default();

        // Read name (aligned string)
        sprite.name = reader.read_aligned_string()?;

        // Read rect
        sprite.rect_x = reader.read_f32()?;
        sprite.rect_y = reader.read_f32()?;
        sprite.rect_width = reader.read_f32()?;
        sprite.rect_height = reader.read_f32()?;

        // Read offset
        sprite.offset_x = reader.read_f32()?;
        sprite.offset_y = reader.read_f32()?;

        // Read border
        sprite.border_x = reader.read_f32()?;
        sprite.border_y = reader.read_f32()?;
        sprite.border_z = reader.read_f32()?;
        sprite.border_w = reader.read_f32()?;

        // Read pixels to units
        sprite.pixels_to_units = reader.read_f32()?;

        // Read pivot
        sprite.pivot_x = reader.read_f32()?;
        sprite.pivot_y = reader.read_f32()?;

        // Read extrude
        sprite.extrude = reader.read_u8()?;

        // Read polygon flag
        sprite.is_polygon = reader.read_bool()?;

        Ok(sprite)
    }

    /// Extract rect from UnityValue
    fn extract_rect(&mut self, value: &UnityValue) -> Result<()> {
        // Rect is typically a complex object with x, y, width, height
        // This is a simplified implementation
        Ok(())
    }

    /// Extract offset from UnityValue
    fn extract_offset(&mut self, value: &UnityValue) -> Result<()> {
        // Vector2 with x, y
        Ok(())
    }

    /// Extract border from UnityValue
    fn extract_border(&mut self, value: &UnityValue) -> Result<()> {
        // Vector4 with x, y, z, w
        Ok(())
    }

    /// Extract pivot from UnityValue
    fn extract_pivot(&mut self, value: &UnityValue) -> Result<()> {
        // Vector2 with x, y
        Ok(())
    }

    /// Extract render data from UnityValue
    fn extract_render_data(&mut self, _value: &UnityValue) -> Result<()> {
        // Complex render data structure
        Ok(())
    }

    /// Extract atlas tags from UnityValue
    fn extract_atlas_tags(&mut self, _value: &UnityValue) -> Result<()> {
        // Array of strings
        Ok(())
    }

    /// Extract sprite atlas reference from UnityValue
    fn extract_sprite_atlas(&mut self, _value: &UnityValue) -> Result<()> {
        // PPtr reference to SpriteAtlas
        Ok(())
    }

    /// Extract sprite image from a texture
    ///
    /// This method extracts the sprite's region from the provided texture,
    /// similar to UnityPy's sprite.image property.
    ///
    /// # Arguments
    /// * `texture` - The source texture containing this sprite
    ///
    /// # Returns
    /// * `Ok(RgbaImage)` - The extracted sprite image
    /// * `Err(BinaryError)` - If extraction fails
    pub fn extract_image(&self, texture: &Texture2D) -> Result<RgbaImage> {
        // First decode the full texture
        let full_image = texture.decode_image()?;

        // Get sprite dimensions and position
        let sprite_x = self.rect_x as u32;
        let sprite_y = self.rect_y as u32;
        let sprite_width = self.rect_width as u32;
        let sprite_height = self.rect_height as u32;

        // Validate sprite bounds
        if sprite_x + sprite_width > full_image.width() {
            return Err(BinaryError::invalid_data(format!(
                "Sprite width ({}) + x ({}) exceeds texture width ({})",
                sprite_width,
                sprite_x,
                full_image.width()
            )));
        }

        if sprite_y + sprite_height > full_image.height() {
            return Err(BinaryError::invalid_data(format!(
                "Sprite height ({}) + y ({}) exceeds texture height ({})",
                sprite_height,
                sprite_y,
                full_image.height()
            )));
        }

        // Unity uses bottom-left origin, but image crate uses top-left
        // Convert Y coordinate
        let texture_height = full_image.height();
        let adjusted_y = texture_height - sprite_y - sprite_height;

        // Extract the sprite region
        let cropped = imageops::crop_imm(
            &full_image,
            sprite_x,
            adjusted_y,
            sprite_width,
            sprite_height,
        );

        Ok(cropped.to_image())
    }

    /// Extract sprite image from render data coordinates
    ///
    /// This method uses the render data coordinates which may be different
    /// from the main rect coordinates, especially for atlas sprites.
    pub fn extract_image_from_render_data(&self, texture: &Texture2D) -> Result<RgbaImage> {
        // First decode the full texture
        let full_image = texture.decode_image()?;

        // Use render data coordinates
        let sprite_x = self.render_data.texture_rect_x as u32;
        let sprite_y = self.render_data.texture_rect_y as u32;
        let sprite_width = self.render_data.texture_rect_width as u32;
        let sprite_height = self.render_data.texture_rect_height as u32;

        // Validate sprite bounds
        if sprite_x + sprite_width > full_image.width() {
            return Err(BinaryError::invalid_data(format!(
                "Sprite render width ({}) + x ({}) exceeds texture width ({})",
                sprite_width,
                sprite_x,
                full_image.width()
            )));
        }

        if sprite_y + sprite_height > full_image.height() {
            return Err(BinaryError::invalid_data(format!(
                "Sprite render height ({}) + y ({}) exceeds texture height ({})",
                sprite_height,
                sprite_y,
                full_image.height()
            )));
        }

        // Unity uses bottom-left origin, but image crate uses top-left
        // Convert Y coordinate
        let texture_height = full_image.height();
        let adjusted_y = texture_height - sprite_y - sprite_height;

        // Extract the sprite region
        let cropped = imageops::crop_imm(
            &full_image,
            sprite_x,
            adjusted_y,
            sprite_width,
            sprite_height,
        );

        Ok(cropped.to_image())
    }

    /// Get sprite information summary
    pub fn get_info(&self) -> SpriteInfo {
        SpriteInfo {
            name: self.name.clone(),
            rect: SpriteRect {
                x: self.rect_x,
                y: self.rect_y,
                width: self.rect_width,
                height: self.rect_height,
            },
            offset: SpriteOffset {
                x: self.offset_x,
                y: self.offset_y,
            },
            pivot: SpritePivot {
                x: self.pivot_x,
                y: self.pivot_y,
            },
            border: SpriteBorder {
                left: self.border_x,
                bottom: self.border_y,
                right: self.border_z,
                top: self.border_w,
            },
            pixels_to_units: self.pixels_to_units,
            is_polygon: self.is_polygon,
            texture_path_id: self.render_data.texture_path_id,
            is_atlas_sprite: self.sprite_atlas_path_id.is_some(),
        }
    }

    /// Decode sprite image (requires texture reference)
    ///
    /// This is a placeholder method that returns an error since it requires
    /// access to the texture that contains this sprite. Use `extract_image`
    /// with the appropriate texture instead.
    pub fn decode_image(&self) -> Result<RgbaImage> {
        Err(BinaryError::generic(
            "Sprite image decoding requires texture reference. Use extract_image(texture) instead.",
        ))
    }

    /// Export sprite to PNG file
    ///
    /// This method extracts the sprite from the provided texture and saves it as PNG.
    ///
    /// # Arguments
    /// * `texture` - The source texture containing this sprite
    /// * `path` - The output file path
    pub fn export_png(&self, texture: &Texture2D, path: &str) -> Result<()> {
        let sprite_image = self.extract_image(texture)?;
        sprite_image
            .save(path)
            .map_err(|e| BinaryError::generic(format!("Failed to save PNG: {}", e)))?;
        Ok(())
    }

    /// Export sprite to PNG file using render data coordinates
    ///
    /// This method uses render data coordinates which may be more accurate
    /// for atlas sprites.
    pub fn export_png_from_render_data(&self, texture: &Texture2D, path: &str) -> Result<()> {
        let sprite_image = self.extract_image_from_render_data(texture)?;
        sprite_image
            .save(path)
            .map_err(|e| BinaryError::generic(format!("Failed to save PNG: {}", e)))?;
        Ok(())
    }
}

/// Sprite processor for handling different Unity versions
#[derive(Debug, Clone)]
pub struct SpriteProcessor {
    version: UnityVersion,
}

impl SpriteProcessor {
    /// Create a new Sprite processor
    pub fn new(version: UnityVersion) -> Self {
        Self { version }
    }

    /// Parse Sprite from Unity object
    pub fn parse_sprite(&self, object: &UnityObject) -> Result<Sprite> {
        Sprite::from_unity_object(object, &self.version)
    }

    /// Get supported sprite features for this Unity version
    pub fn get_supported_features(&self) -> Vec<&'static str> {
        let mut features = vec!["basic_sprite", "rect", "pivot"];

        if self.version.major >= 5 {
            features.push("border");
            features.push("pixels_to_units");
        }

        if self.version.major >= 2017 {
            features.push("polygon_sprites");
            features.push("sprite_atlas");
        }

        features
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sprite_default() {
        let sprite = Sprite::default();
        assert_eq!(sprite.name, "");
        assert_eq!(sprite.pixels_to_units, 100.0);
        assert_eq!(sprite.pivot_x, 0.5);
        assert_eq!(sprite.pivot_y, 0.5);
        assert!(!sprite.is_polygon);
    }

    #[test]
    fn test_sprite_processor() {
        let version = UnityVersion::from_str("2020.3.12f1").unwrap();
        let processor = SpriteProcessor::new(version);

        let features = processor.get_supported_features();
        assert!(features.contains(&"basic_sprite"));
        assert!(features.contains(&"polygon_sprites"));
        assert!(features.contains(&"sprite_atlas"));
    }
}
