//! Texture2D converter and processor
//!
//! This module provides the main conversion logic for Unity Texture2D objects.
//! Inspired by UnityPy/export/Texture2DConverter.py

use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::unity_version::UnityVersion;
use super::types::Texture2D;
use super::decoders::TextureDecoder;
use image::RgbaImage;

/// Main texture converter
///
/// This struct handles the conversion of Unity objects to Texture2D structures
/// and provides methods for processing texture data.
pub struct Texture2DConverter {
    version: UnityVersion,
    decoder: TextureDecoder,
}

impl Texture2DConverter {
    /// Create a new Texture2D converter
    pub fn new(version: UnityVersion) -> Self {
        Self {
            version,
            decoder: TextureDecoder::new(),
        }
    }

    /// Convert Unity object to Texture2D
    ///
    /// This method extracts texture data from a Unity object and creates
    /// a Texture2D structure with all necessary metadata.
    pub fn from_unity_object(&self, obj: &UnityObject) -> Result<Texture2D> {
        // For now, use a simplified approach similar to the old implementation
        // TODO: Implement proper TypeTree parsing when available
        self.from_binary_data(&obj.info.data)
    }

    /// Parse Texture2D from raw binary data (simplified version)
    fn from_binary_data(&self, data: &[u8]) -> Result<Texture2D> {
        if data.is_empty() {
            return Err(BinaryError::invalid_data("Empty texture data"));
        }

        let mut reader = crate::reader::BinaryReader::new(data, crate::reader::ByteOrder::Little);
        let mut texture = Texture2D::default();

        // Read name first
        texture.name = reader
            .read_aligned_string()
            .unwrap_or_else(|_| "UnknownTexture".to_string());

        // Core dimensions and format
        texture.width = reader.read_i32().unwrap_or(0);
        texture.height = reader.read_i32().unwrap_or(0);
        texture.complete_image_size = reader.read_i32().unwrap_or(0);

        let format_val = reader.read_i32().unwrap_or(0);
        texture.format = super::formats::TextureFormat::from(format_val);

        // Basic flags
        texture.mip_map = reader.read_bool().unwrap_or(false);
        texture.is_readable = reader.read_bool().unwrap_or(false);

        // Read data size and image data
        texture.data_size = reader.read_i32().unwrap_or(0);

        // Read actual image data
        if texture.data_size > 0 && reader.remaining() >= texture.data_size as usize {
            texture.image_data = reader
                .read_bytes(texture.data_size as usize)
                .unwrap_or_default();
        } else if reader.remaining() > 0 {
            // Fallback: take all remaining data
            let remaining_data = reader.read_remaining();
            texture.image_data = remaining_data.to_vec();
            texture.data_size = texture.image_data.len() as i32;
        }

        Ok(texture)
    }

    /// Decode texture to RGBA image
    ///
    /// This method uses the texture decoder to convert texture data to RGBA format
    pub fn decode_to_image(&self, texture: &Texture2D) -> Result<RgbaImage> {
        // Use the texture decoder to decode the image
        self.decoder.decode(texture)
    }


}

// Legacy compatibility - alias for the old processor name
pub type Texture2DProcessor = Texture2DConverter;
