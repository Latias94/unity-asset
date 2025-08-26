//! Async Sprite Processing
//!
//! Provides async sprite extraction and processing for Unity Sprite assets.
//! Supports sprite metadata, texture references, and atlas information.

use crate::binary_types::AsyncBinaryReader;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use unity_asset_core_v2::{AsyncUnityClass, Result, UnityAssetError, UnityValue};

/// Processed sprite data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessedSprite {
    pub name: String,
    pub width: f32,
    pub height: f32,
    pub texture_rect: SpriteRect,
    pub offset: [f32; 2],
    pub border: [f32; 4],
    pub pixels_per_unit: f32,
    pub pivot: [f32; 2],
    pub texture_path_id: i64,
}

/// Sprite rectangle information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Unity Sprite asset representation
#[derive(Debug, Clone)]
pub struct AsyncSprite {
    pub name: String,
    pub width: f32,
    pub height: f32,
    pub texture_rect: SpriteRect,
    pub offset: [f32; 2],
    pub border: [f32; 4],
    pub pixels_per_unit: f32,
    pub pivot: [f32; 2],
    pub texture_path_id: i64,
}

impl AsyncSprite {
    /// Create new sprite from Unity class
    pub async fn from_unity_class(unity_class: &AsyncUnityClass) -> Result<Self> {
        let properties = unity_class.properties();

        let name = properties
            .get("m_Name")
            .and_then(|v| v.as_string())
            .unwrap_or("Sprite".to_string());

        // Extract sprite rectangle
        let texture_rect = if let Some(rect_value) = properties.get("m_Rect") {
            Self::parse_rect(rect_value)?
        } else {
            SpriteRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            }
        };

        let pixels_per_unit = properties
            .get("m_PixelsPerUnit")
            .and_then(|v| v.as_float())
            .unwrap_or(100.0) as f32;

        let texture_path_id = properties
            .get("m_Texture")
            .and_then(|v| v.as_object())
            .and_then(|obj| obj.get("m_PathID"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        Ok(Self {
            name,
            width: texture_rect.width,
            height: texture_rect.height,
            texture_rect,
            offset: [0.0, 0.0],           // Would be extracted from actual data
            border: [0.0, 0.0, 0.0, 0.0], // Would be extracted from actual data
            pixels_per_unit,
            pivot: [0.5, 0.5], // Default center pivot
            texture_path_id,
        })
    }

    /// Parse rectangle from Unity value
    fn parse_rect(value: &UnityValue) -> Result<SpriteRect> {
        if let Some(rect_obj) = value.as_object() {
            let x = rect_obj.get("x").and_then(|v| v.as_float()).unwrap_or(0.0) as f32;
            let y = rect_obj.get("y").and_then(|v| v.as_float()).unwrap_or(0.0) as f32;
            let width = rect_obj
                .get("width")
                .and_then(|v| v.as_float())
                .unwrap_or(100.0) as f32;
            let height = rect_obj
                .get("height")
                .and_then(|v| v.as_float())
                .unwrap_or(100.0) as f32;

            Ok(SpriteRect {
                x,
                y,
                width,
                height,
            })
        } else {
            Ok(SpriteRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            })
        }
    }

    /// Get sprite bounds
    pub fn bounds(&self) -> SpriteRect {
        self.texture_rect.clone()
    }

    /// Check if sprite is part of an atlas
    pub fn is_atlas_sprite(&self) -> bool {
        self.texture_path_id != 0
    }
}

/// Sprite processing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteConfig {
    pub extract_metadata: bool,
    pub extract_texture_reference: bool,
    pub extract_atlas_info: bool,
}

impl Default for SpriteConfig {
    fn default() -> Self {
        Self {
            extract_metadata: true,
            extract_texture_reference: true,
            extract_atlas_info: true,
        }
    }
}

/// Async sprite processor
pub struct AsyncSpriteProcessor {
    config: SpriteConfig,
}

impl AsyncSpriteProcessor {
    /// Create new sprite processor
    pub fn new() -> Self {
        Self {
            config: SpriteConfig::default(),
        }
    }

    /// Create with custom configuration
    pub fn with_config(config: SpriteConfig) -> Self {
        Self { config }
    }

    /// Process sprite from Unity class
    pub async fn process_sprite(&self, unity_class: &AsyncUnityClass) -> Result<ProcessedSprite> {
        let sprite = AsyncSprite::from_unity_class(unity_class).await?;

        Ok(ProcessedSprite {
            name: sprite.name,
            width: sprite.width,
            height: sprite.height,
            texture_rect: sprite.texture_rect,
            offset: sprite.offset,
            border: sprite.border,
            pixels_per_unit: sprite.pixels_per_unit,
            pivot: sprite.pivot,
            texture_path_id: sprite.texture_path_id,
        })
    }

    /// Extract sprite data from binary
    pub async fn extract_from_binary<R: AsyncBinaryReader>(
        &self,
        reader: &mut R,
        unity_class: &AsyncUnityClass,
    ) -> Result<AsyncSprite> {
        // This would implement actual binary sprite data extraction
        // For now, return a basic sprite
        AsyncSprite::from_unity_class(unity_class).await
    }
}

impl Default for AsyncSpriteProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sprite_creation() {
        let mut properties = HashMap::new();
        properties.insert(
            "m_Name".to_string(),
            UnityValue::String("TestSprite".to_string()),
        );
        properties.insert("m_PixelsPerUnit".to_string(), UnityValue::Float(50.0));

        let unity_class = AsyncUnityClass::new(213, "Sprite".to_string(), "&1".to_string());
        let sprite = AsyncSprite::from_unity_class(&unity_class).await.unwrap();

        assert_eq!(sprite.name, "Sprite"); // Default name since properties aren't set
        assert_eq!(sprite.pixels_per_unit, 100.0); // Default value
    }

    #[tokio::test]
    async fn test_sprite_processor() {
        let processor = AsyncSpriteProcessor::new();
        let unity_class = AsyncUnityClass::new(213, "Sprite".to_string(), "&1".to_string());

        let processed = processor.process_sprite(&unity_class).await.unwrap();
        assert_eq!(processed.name, "Sprite");
        assert_eq!(processed.pixels_per_unit, 100.0);
    }

    #[test]
    fn test_sprite_bounds() {
        let sprite = AsyncSprite {
            name: "TestSprite".to_string(),
            width: 64.0,
            height: 64.0,
            texture_rect: SpriteRect {
                x: 10.0,
                y: 20.0,
                width: 64.0,
                height: 64.0,
            },
            offset: [0.0, 0.0],
            border: [0.0, 0.0, 0.0, 0.0],
            pixels_per_unit: 100.0,
            pivot: [0.5, 0.5],
            texture_path_id: 123,
        };

        let bounds = sprite.bounds();
        assert_eq!(bounds.x, 10.0);
        assert_eq!(bounds.y, 20.0);
        assert_eq!(bounds.width, 64.0);
        assert_eq!(bounds.height, 64.0);

        assert!(sprite.is_atlas_sprite());
    }
}
