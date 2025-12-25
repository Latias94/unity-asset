//! Crunch compressed texture format decoders
//!
//! This module handles Crunch-compressed texture formats.
//! Crunch is Unity's proprietary compression that can wrap other formats like DXT.

use super::{Decoder, create_rgba_image, validate_dimensions};
use crate::error::{BinaryError, Result};
use crate::texture::formats::TextureFormat;
use crate::texture::types::Texture2D;
use image::RgbaImage;

/// Decoder for Crunch compressed texture formats
pub struct CrunchDecoder;

impl CrunchDecoder {
    /// Create a new Crunch decoder
    pub fn new() -> Self {
        Self
    }

    /// Decompress Crunch compressed data
    #[cfg(feature = "texture-advanced")]
    fn decompress_crunch(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
        validate_dimensions(width, height)?;

        let mut output = vec![0u32; (width * height) as usize];

        match texture2ddecoder::decode_crunch(data, width as usize, height as usize, &mut output) {
            Ok(_) => {
                // Convert u32 RGBA to u8 RGBA
                let rgba_data: Vec<u8> = output
                    .iter()
                    .flat_map(|&pixel| {
                        [
                            (pixel & 0xFF) as u8,         // R
                            ((pixel >> 8) & 0xFF) as u8,  // G
                            ((pixel >> 16) & 0xFF) as u8, // B
                            ((pixel >> 24) & 0xFF) as u8, // A
                        ]
                    })
                    .collect();
                Ok(rgba_data)
            }
            Err(e) => Err(BinaryError::generic(format!(
                "Crunch decompression failed: {}",
                e
            ))),
        }
    }

    /// Decode DXT1 Crunched format
    #[cfg(feature = "texture-advanced")]
    fn decode_dxt1_crunched(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let rgba_data = self.decompress_crunch(data, width, height)?;
        create_rgba_image(rgba_data, width, height)
    }

    /// Decode DXT5 Crunched format
    #[cfg(feature = "texture-advanced")]
    fn decode_dxt5_crunched(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let rgba_data = self.decompress_crunch(data, width, height)?;
        create_rgba_image(rgba_data, width, height)
    }

    /// Decode ETC RGB4 Crunched format
    #[cfg(feature = "texture-advanced")]
    fn decode_etc_rgb4_crunched(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let rgba_data = self.decompress_crunch(data, width, height)?;
        create_rgba_image(rgba_data, width, height)
    }

    /// Decode ETC2 RGBA8 Crunched format
    #[cfg(feature = "texture-advanced")]
    fn decode_etc2_rgba8_crunched(
        &self,
        data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<RgbaImage> {
        let rgba_data = self.decompress_crunch(data, width, height)?;
        create_rgba_image(rgba_data, width, height)
    }

    /// Fallback for when texture-advanced feature is not enabled
    #[cfg(not(feature = "texture-advanced"))]
    fn decode_unsupported(&self, format: TextureFormat) -> Result<RgbaImage> {
        Err(BinaryError::unsupported(format!(
            "Crunch format {:?} requires texture-advanced feature",
            format
        )))
    }
}

impl Decoder for CrunchDecoder {
    fn decode(&self, texture: &Texture2D) -> Result<RgbaImage> {
        let width = texture.width as u32;
        let height = texture.height as u32;
        let data = &texture.image_data;

        match texture.format {
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT1Crunched => self.decode_dxt1_crunched(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT5Crunched => self.decode_dxt5_crunched(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC_RGB4Crunched => self.decode_etc_rgb4_crunched(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGBA8Crunched => {
                self.decode_etc2_rgba8_crunched(data, width, height)
            }

            #[cfg(not(feature = "texture-advanced"))]
            format if format.is_crunch_compressed() => self.decode_unsupported(format),

            _ => Err(BinaryError::unsupported(format!(
                "Format {:?} is not a Crunch format",
                texture.format
            ))),
        }
    }

    fn can_decode(&self, format: TextureFormat) -> bool {
        #[cfg(feature = "texture-advanced")]
        {
            format.is_crunch_compressed()
        }

        #[cfg(not(feature = "texture-advanced"))]
        {
            false
        }
    }

    fn supported_formats(&self) -> Vec<TextureFormat> {
        #[cfg(feature = "texture-advanced")]
        {
            vec![
                TextureFormat::DXT1Crunched,
                TextureFormat::DXT5Crunched,
                TextureFormat::ETC_RGB4Crunched,
                TextureFormat::ETC2_RGBA8Crunched,
            ]
        }

        #[cfg(not(feature = "texture-advanced"))]
        {
            vec![]
        }
    }
}

impl Default for CrunchDecoder {
    fn default() -> Self {
        Self::new()
    }
}
