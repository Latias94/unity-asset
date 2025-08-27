//! Texture data structures
//!
//! This module defines the core data structures used for texture processing.

use serde::{Deserialize, Serialize};
use super::formats::TextureFormat;

/// Streaming info for external texture data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamingInfo {
    pub offset: u64,
    pub size: u32,
    pub path: String,
}

/// GL texture settings
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GLTextureSettings {
    pub filter_mode: i32,
    pub aniso: i32,
    pub mip_bias: f32,
    pub wrap_u: i32,
    pub wrap_v: i32,
    pub wrap_w: i32,
}

/// Texture2D object representation
/// 
/// This structure contains all the data needed to represent a Unity Texture2D object.
/// It includes both metadata and the actual image data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Texture2D {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub complete_image_size: i32,
    pub format: TextureFormat,
    pub mip_map: bool,
    pub mip_count: i32,
    pub is_readable: bool,
    pub image_count: i32,
    pub texture_dimension: i32,
    pub light_map_format: i32,
    pub color_space: i32,
    pub data_size: i32,
    pub stream_info: StreamingInfo,
    pub texture_settings: GLTextureSettings,
    pub image_data: Vec<u8>,

    // Version-specific fields
    pub forced_fallback_format: Option<i32>,
    pub downscale_fallback: Option<bool>,
    pub is_alpha_channel_optional: Option<bool>,
    pub mips_stripped: Option<i32>,
}

impl Default for Texture2D {
    fn default() -> Self {
        Self {
            name: String::new(),
            width: 0,
            height: 0,
            complete_image_size: 0,
            format: TextureFormat::Unknown,
            mip_map: false,
            mip_count: 1,
            is_readable: false,
            image_count: 1,
            texture_dimension: 2,
            light_map_format: 0,
            color_space: 0,
            data_size: 0,
            stream_info: StreamingInfo::default(),
            texture_settings: GLTextureSettings::default(),
            image_data: Vec::new(),
            forced_fallback_format: None,
            downscale_fallback: None,
            is_alpha_channel_optional: None,
            mips_stripped: None,
        }
    }
}

impl Texture2D {
    /// Create a new Texture2D with basic parameters
    pub fn new(name: String, width: i32, height: i32, format: TextureFormat) -> Self {
        Self {
            name,
            width,
            height,
            format,
            ..Default::default()
        }
    }

    /// Check if texture has valid dimensions
    pub fn has_valid_dimensions(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// Check if texture has image data
    pub fn has_image_data(&self) -> bool {
        !self.image_data.is_empty()
    }

    /// Get texture dimensions as tuple
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    /// Check if texture uses external streaming
    pub fn is_streamed(&self) -> bool {
        !self.stream_info.path.is_empty() && self.stream_info.size > 0
    }

    /// Get expected data size based on format and dimensions
    pub fn expected_data_size(&self) -> u32 {
        self.format.calculate_data_size(self.width as u32, self.height as u32)
    }

    /// Validate texture data consistency
    pub fn validate(&self) -> Result<(), String> {
        if !self.has_valid_dimensions() {
            return Err("Invalid texture dimensions".to_string());
        }

        if !self.format.is_supported() {
            return Err(format!("Unsupported texture format: {:?}", self.format));
        }

        if !self.is_streamed() && !self.has_image_data() {
            return Err("No image data available and not streamed".to_string());
        }

        Ok(())
    }
}
