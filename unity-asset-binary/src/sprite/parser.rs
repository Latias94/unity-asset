//! Sprite parsing implementation
//!
//! This module provides the main parsing logic for Unity Sprite objects.

use super::types::*;
use crate::error::Result;
use crate::object::UnityObject;
use crate::reader::BinaryReader;
use crate::unity_version::UnityVersion;
use indexmap::IndexMap;
use unity_asset_core::UnityValue;

/// Sprite parser
///
/// This struct provides methods for parsing Unity Sprite objects from
/// various data sources including TypeTree and binary data.
pub struct SpriteParser {
    version: UnityVersion,
}

impl SpriteParser {
    /// Create a new sprite parser
    pub fn new(version: UnityVersion) -> Self {
        Self { version }
    }

    /// Parse Sprite from UnityObject
    pub fn parse_from_unity_object(&self, obj: &UnityObject) -> Result<SpriteResult> {
        let sprite = if let Some(type_tree) = &obj.info.type_tree {
            let properties = obj.parse_with_typetree(type_tree)?;
            self.parse_from_typetree(&properties)?
        } else {
            self.parse_from_binary_data(&obj.info.data)?
        };

        Ok(SpriteResult::new(sprite))
    }

    /// Parse Sprite from TypeTree properties
    pub fn parse_from_typetree(&self, properties: &IndexMap<String, UnityValue>) -> Result<Sprite> {
        let mut sprite = Sprite::default();

        // Extract name
        if let Some(UnityValue::String(name)) = properties.get("m_Name") {
            sprite.name = name.clone();
        }

        // Extract rect
        if let Some(rect_value) = properties.get("m_Rect") {
            self.extract_rect(&mut sprite, rect_value)?;
        }

        // Extract offset
        if let Some(offset_value) = properties.get("m_Offset") {
            self.extract_offset(&mut sprite, offset_value)?;
        }

        // Extract border
        if let Some(border_value) = properties.get("m_Border") {
            self.extract_border(&mut sprite, border_value)?;
        }

        // Extract pixels to units
        if let Some(UnityValue::Float(pixels_to_units)) = properties.get("m_PixelsToUnits") {
            sprite.pixels_to_units = *pixels_to_units as f32;
        }

        // Extract pivot
        if let Some(pivot_value) = properties.get("m_Pivot") {
            self.extract_pivot(&mut sprite, pivot_value)?;
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
            self.extract_render_data(&mut sprite, render_data_value)?;
        }

        // Extract atlas tags
        if let Some(atlas_tags_value) = properties.get("m_AtlasTags") {
            self.extract_atlas_tags(&mut sprite, atlas_tags_value)?;
        }

        // Extract sprite atlas reference
        if let Some(sprite_atlas_value) = properties.get("m_SpriteAtlas") {
            self.extract_sprite_atlas(&mut sprite, sprite_atlas_value)?;
        }

        Ok(sprite)
    }

    /// Parse Sprite from raw binary data (fallback method)
    pub fn parse_from_binary_data(&self, data: &[u8]) -> Result<Sprite> {
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
        reader.align_to(4)?; // Align after reading byte

        // Read polygon flag
        sprite.is_polygon = reader.read_bool()?;
        reader.align_to(4)?; // Align after reading bool

        // Try to read render data if there's more data
        if reader.remaining() > 0 {
            self.read_render_data_binary(&mut sprite, &mut reader)?;
        }

        Ok(sprite)
    }

    /// Extract rectangle from UnityValue
    fn extract_rect(&self, sprite: &mut Sprite, rect_value: &UnityValue) -> Result<()> {
        if let UnityValue::Object(rect_obj) = rect_value {
            if let Some(UnityValue::Float(x)) = rect_obj.get("x") {
                sprite.rect_x = *x as f32;
            }
            if let Some(UnityValue::Float(y)) = rect_obj.get("y") {
                sprite.rect_y = *y as f32;
            }
            if let Some(UnityValue::Float(width)) = rect_obj.get("width") {
                sprite.rect_width = *width as f32;
            }
            if let Some(UnityValue::Float(height)) = rect_obj.get("height") {
                sprite.rect_height = *height as f32;
            }
        }
        Ok(())
    }

    /// Extract offset from UnityValue
    fn extract_offset(&self, sprite: &mut Sprite, offset_value: &UnityValue) -> Result<()> {
        if let UnityValue::Object(offset_obj) = offset_value {
            if let Some(UnityValue::Float(x)) = offset_obj.get("x") {
                sprite.offset_x = *x as f32;
            }
            if let Some(UnityValue::Float(y)) = offset_obj.get("y") {
                sprite.offset_y = *y as f32;
            }
        }
        Ok(())
    }

    /// Extract border from UnityValue
    fn extract_border(&self, sprite: &mut Sprite, border_value: &UnityValue) -> Result<()> {
        if let UnityValue::Object(border_obj) = border_value {
            if let Some(UnityValue::Float(x)) = border_obj.get("x") {
                sprite.border_x = *x as f32;
            }
            if let Some(UnityValue::Float(y)) = border_obj.get("y") {
                sprite.border_y = *y as f32;
            }
            if let Some(UnityValue::Float(z)) = border_obj.get("z") {
                sprite.border_z = *z as f32;
            }
            if let Some(UnityValue::Float(w)) = border_obj.get("w") {
                sprite.border_w = *w as f32;
            }
        }
        Ok(())
    }

    /// Extract pivot from UnityValue
    fn extract_pivot(&self, sprite: &mut Sprite, pivot_value: &UnityValue) -> Result<()> {
        if let UnityValue::Object(pivot_obj) = pivot_value {
            if let Some(UnityValue::Float(x)) = pivot_obj.get("x") {
                sprite.pivot_x = *x as f32;
            }
            if let Some(UnityValue::Float(y)) = pivot_obj.get("y") {
                sprite.pivot_y = *y as f32;
            }
        }
        Ok(())
    }

    /// Extract render data from UnityValue
    fn extract_render_data(
        &self,
        sprite: &mut Sprite,
        render_data_value: &UnityValue,
    ) -> Result<()> {
        if let UnityValue::Object(rd_obj) = render_data_value {
            // Extract texture reference
            if let Some(texture_value) = rd_obj.get("texture") {
                if let UnityValue::Object(texture_obj) = texture_value {
                    if let Some(UnityValue::Integer(file_id)) = texture_obj.get("m_FileID") {
                        sprite.render_data.texture_path_id = *file_id;
                    }
                }
            }

            // Extract texture rect
            if let Some(texture_rect_value) = rd_obj.get("textureRect") {
                if let UnityValue::Object(rect_obj) = texture_rect_value {
                    if let Some(UnityValue::Float(x)) = rect_obj.get("x") {
                        sprite.render_data.texture_rect_x = *x as f32;
                    }
                    if let Some(UnityValue::Float(y)) = rect_obj.get("y") {
                        sprite.render_data.texture_rect_y = *y as f32;
                    }
                    if let Some(UnityValue::Float(width)) = rect_obj.get("width") {
                        sprite.render_data.texture_rect_width = *width as f32;
                    }
                    if let Some(UnityValue::Float(height)) = rect_obj.get("height") {
                        sprite.render_data.texture_rect_height = *height as f32;
                    }
                }
            }

            // Extract other render data fields
            if let Some(UnityValue::Float(downscale)) = rd_obj.get("downscaleMultiplier") {
                sprite.render_data.downscale_multiplier = *downscale as f32;
            }
        }
        Ok(())
    }

    /// Extract atlas tags from UnityValue
    fn extract_atlas_tags(&self, sprite: &mut Sprite, atlas_tags_value: &UnityValue) -> Result<()> {
        if let UnityValue::Array(tags_array) = atlas_tags_value {
            sprite.atlas_tags.clear();
            for tag_value in tags_array {
                if let UnityValue::String(tag) = tag_value {
                    sprite.atlas_tags.push(tag.clone());
                }
            }
        }
        Ok(())
    }

    /// Extract sprite atlas reference from UnityValue
    fn extract_sprite_atlas(
        &self,
        sprite: &mut Sprite,
        sprite_atlas_value: &UnityValue,
    ) -> Result<()> {
        if let UnityValue::Object(atlas_obj) = sprite_atlas_value {
            if let Some(UnityValue::Integer(path_id)) = atlas_obj.get("m_PathID") {
                sprite.sprite_atlas_path_id = Some(*path_id);
            }
        }
        Ok(())
    }

    /// Read render data from binary stream
    fn read_render_data_binary(
        &self,
        sprite: &mut Sprite,
        reader: &mut BinaryReader,
    ) -> Result<()> {
        // This is a simplified implementation
        // The actual structure depends on the Unity version
        if reader.remaining() >= 4 {
            sprite.render_data.texture_path_id = reader.read_i64().unwrap_or(0);
        }

        if reader.remaining() >= 16 {
            sprite.render_data.texture_rect_x = reader.read_f32().unwrap_or(0.0);
            sprite.render_data.texture_rect_y = reader.read_f32().unwrap_or(0.0);
            sprite.render_data.texture_rect_width = reader.read_f32().unwrap_or(0.0);
            sprite.render_data.texture_rect_height = reader.read_f32().unwrap_or(0.0);
        }

        Ok(())
    }

    /// Get the Unity version
    pub fn version(&self) -> &UnityVersion {
        &self.version
    }

    /// Set the Unity version
    pub fn set_version(&mut self, version: UnityVersion) {
        self.version = version;
    }
}

impl Default for SpriteParser {
    fn default() -> Self {
        Self::new(UnityVersion::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_creation() {
        let parser = SpriteParser::new(UnityVersion::default());
        assert_eq!(parser.version(), &UnityVersion::default());
    }

    #[test]
    fn test_extract_rect() {
        let parser = SpriteParser::default();
        let mut sprite = Sprite::default();

        let mut rect_obj = IndexMap::new();
        rect_obj.insert("x".to_string(), UnityValue::Float(10.0));
        rect_obj.insert("y".to_string(), UnityValue::Float(20.0));
        rect_obj.insert("width".to_string(), UnityValue::Float(100.0));
        rect_obj.insert("height".to_string(), UnityValue::Float(200.0));

        let rect_value = UnityValue::Object(rect_obj);
        parser.extract_rect(&mut sprite, &rect_value).unwrap();

        assert_eq!(sprite.rect_x, 10.0);
        assert_eq!(sprite.rect_y, 20.0);
        assert_eq!(sprite.rect_width, 100.0);
        assert_eq!(sprite.rect_height, 200.0);
    }
}
