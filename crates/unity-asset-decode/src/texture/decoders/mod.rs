//! Texture decoders module
//!
//! This module provides specialized decoders for different texture formats,
//! organized by format category for better maintainability.

mod basic;
mod compressed;
mod crunch;
mod mobile;

pub use basic::BasicDecoder;
pub use compressed::CompressedDecoder;
pub use crunch::CrunchDecoder;
pub use mobile::MobileDecoder;

use super::formats::TextureFormat;
use super::types::Texture2D;
use crate::error::{BinaryError, Result};
use image::RgbaImage;

/// Main texture decoder dispatcher
///
/// This struct coordinates between different specialized decoders
/// based on the texture format type.
pub struct TextureDecoder {
    basic: BasicDecoder,
    compressed: CompressedDecoder,
    mobile: MobileDecoder,
    crunch: CrunchDecoder,
}

impl TextureDecoder {
    /// Create a new texture decoder
    pub fn new() -> Self {
        Self {
            basic: BasicDecoder::new(),
            compressed: CompressedDecoder::new(),
            mobile: MobileDecoder::new(),
            crunch: CrunchDecoder::new(),
        }
    }

    /// Decode texture to RGBA image
    ///
    /// This method dispatches to the appropriate specialized decoder
    /// based on the texture format.
    pub fn decode(&self, texture: &Texture2D) -> Result<RgbaImage> {
        // Validate texture first
        texture
            .validate()
            .map_err(|e| BinaryError::invalid_data(&e))?;

        // Handle Crunch compression first (it can wrap other formats)
        if texture.format.is_crunch_compressed() {
            return self.crunch.decode(texture);
        }

        // Dispatch to appropriate decoder based on format category
        if texture.format.is_basic_format() {
            self.basic.decode(texture)
        } else if texture.format.is_compressed_format() {
            self.compressed.decode(texture)
        } else if texture.format.is_mobile_format() {
            self.mobile.decode(texture)
        } else {
            Err(BinaryError::unsupported(format!(
                "Unsupported texture format: {:?}",
                texture.format
            )))
        }
    }

    /// Check if a format can be decoded
    pub fn can_decode(&self, format: TextureFormat) -> bool {
        format.is_basic_format()
            || format.is_compressed_format()
            || format.is_mobile_format()
            || format.is_crunch_compressed()
    }

    /// Get list of supported formats
    pub fn supported_formats(&self) -> Vec<TextureFormat> {
        vec![
            // Basic formats
            TextureFormat::Alpha8,
            TextureFormat::RGB24,
            TextureFormat::RGBA32,
            TextureFormat::ARGB32,
            TextureFormat::BGRA32,
            TextureFormat::RGBA4444,
            TextureFormat::ARGB4444,
            TextureFormat::RGB565,
            // Compressed formats (when texture-advanced feature is enabled)
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT1,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT5,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::BC7,
            // Mobile formats (when texture-advanced feature is enabled)
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGB,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGBA8,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_4x4,
            // Crunch formats (when texture-advanced feature is enabled)
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT1Crunched,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT5Crunched,
        ]
    }
}

impl Default for TextureDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Common decoder trait
///
/// This trait defines the interface that all specialized decoders must implement.
pub trait Decoder {
    /// Decode texture data to RGBA image
    fn decode(&self, texture: &Texture2D) -> Result<RgbaImage>;

    /// Check if this decoder can handle the given format
    fn can_decode(&self, format: TextureFormat) -> bool;

    /// Get list of formats supported by this decoder
    fn supported_formats(&self) -> Vec<TextureFormat>;
}

/// Helper function to create RGBA image from raw data
pub(crate) fn create_rgba_image(data: Vec<u8>, width: u32, height: u32) -> Result<RgbaImage> {
    if data.len() != (width * height * 4) as usize {
        return Err(BinaryError::invalid_data(format!(
            "Invalid data size: expected {}, got {}",
            width * height * 4,
            data.len()
        )));
    }

    RgbaImage::from_raw(width, height, data)
        .ok_or_else(|| BinaryError::invalid_data("Failed to create RGBA image from raw data"))
}

/// Helper function to validate dimensions
pub(crate) fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(BinaryError::invalid_data("Invalid texture dimensions"));
    }

    // Reasonable size limits to prevent memory issues
    if width > 16384 || height > 16384 {
        return Err(BinaryError::invalid_data("Texture dimensions too large"));
    }

    Ok(())
}
