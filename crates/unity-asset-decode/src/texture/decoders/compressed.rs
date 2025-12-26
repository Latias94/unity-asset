//! Compressed texture format decoders
//!
//! This module handles compressed texture formats like DXT1, DXT5, BC7, etc.
//! Requires the texture-advanced feature for texture2ddecoder integration.

use super::{Decoder, create_rgba_image, validate_dimensions};
use crate::error::{BinaryError, Result};
use crate::texture::formats::TextureFormat;
use crate::texture::types::Texture2D;
use image::RgbaImage;

/// Decoder for compressed texture formats
pub struct CompressedDecoder;

impl CompressedDecoder {
    /// Create a new compressed decoder
    pub fn new() -> Self {
        Self
    }

    /// Decode DXT1 format
    #[cfg(feature = "texture-advanced")]
    fn decode_dxt1(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let mut output = vec![0u32; (width * height) as usize];

        match texture2ddecoder::decode_bc1(data, width as usize, height as usize, &mut output) {
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

                create_rgba_image(rgba_data, width, height)
            }
            Err(e) => Err(BinaryError::generic(format!("DXT1 decoding failed: {}", e))),
        }
    }

    /// Decode DXT5 format
    #[cfg(feature = "texture-advanced")]
    fn decode_dxt5(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let mut output = vec![0u32; (width * height) as usize];

        match texture2ddecoder::decode_bc3(data, width as usize, height as usize, &mut output) {
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

                create_rgba_image(rgba_data, width, height)
            }
            Err(e) => Err(BinaryError::generic(format!("DXT5 decoding failed: {}", e))),
        }
    }

    /// Decode BC7 format
    #[cfg(feature = "texture-advanced")]
    fn decode_bc7(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let mut output = vec![0u32; (width * height) as usize];

        match texture2ddecoder::decode_bc7(data, width as usize, height as usize, &mut output) {
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

                create_rgba_image(rgba_data, width, height)
            }
            Err(e) => Err(BinaryError::generic(format!("BC7 decoding failed: {}", e))),
        }
    }

    /// Decode BC4 format (single channel)
    #[cfg(feature = "texture-advanced")]
    fn decode_bc4(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let mut output = vec![0u32; (width * height) as usize];

        match texture2ddecoder::decode_bc4(data, width as usize, height as usize, &mut output) {
            Ok(_) => {
                // Convert u32 to u8 RGBA (BC4 is single channel, so replicate to RGB)
                let rgba_data: Vec<u8> = output
                    .iter()
                    .flat_map(|&pixel| {
                        let value = (pixel & 0xFF) as u8;
                        [value, value, value, 255] // Replicate to RGB, full alpha
                    })
                    .collect();

                create_rgba_image(rgba_data, width, height)
            }
            Err(e) => Err(BinaryError::generic(format!("BC4 decoding failed: {}", e))),
        }
    }

    /// Decode BC5 format (two channel)
    #[cfg(feature = "texture-advanced")]
    fn decode_bc5(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let mut output = vec![0u32; (width * height) as usize];

        match texture2ddecoder::decode_bc5(data, width as usize, height as usize, &mut output) {
            Ok(_) => {
                // Convert u32 to u8 RGBA (BC5 has RG channels)
                let rgba_data: Vec<u8> = output
                    .iter()
                    .flat_map(|&pixel| {
                        [
                            (pixel & 0xFF) as u8,        // R
                            ((pixel >> 8) & 0xFF) as u8, // G
                            0,                           // B (not present in BC5)
                            255,                         // A (full alpha)
                        ]
                    })
                    .collect();

                create_rgba_image(rgba_data, width, height)
            }
            Err(e) => Err(BinaryError::generic(format!("BC5 decoding failed: {}", e))),
        }
    }

    /// Fallback for when texture-advanced feature is not enabled
    #[cfg(not(feature = "texture-advanced"))]
    fn decode_unsupported(&self, format: TextureFormat) -> Result<RgbaImage> {
        Err(BinaryError::unsupported(format!(
            "Compressed format {:?} requires texture-advanced feature",
            format
        )))
    }
}

impl Decoder for CompressedDecoder {
    fn decode(&self, texture: &Texture2D) -> Result<RgbaImage> {
        let width = texture.width as u32;
        let height = texture.height as u32;
        let data = &texture.image_data;

        match texture.format {
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT1 => self.decode_dxt1(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT5 => self.decode_dxt5(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::BC7 => self.decode_bc7(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::BC4 => self.decode_bc4(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::BC5 => self.decode_bc5(data, width, height),

            #[cfg(not(feature = "texture-advanced"))]
            format if format.is_compressed_format() => self.decode_unsupported(format),

            _ => Err(BinaryError::unsupported(format!(
                "Format {:?} is not a compressed format",
                texture.format
            ))),
        }
    }

    fn can_decode(&self, format: TextureFormat) -> bool {
        #[cfg(feature = "texture-advanced")]
        {
            matches!(
                format,
                TextureFormat::DXT1
                    | TextureFormat::DXT5
                    | TextureFormat::BC4
                    | TextureFormat::BC5
                    | TextureFormat::BC7
            )
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
                TextureFormat::DXT1,
                TextureFormat::DXT5,
                TextureFormat::BC4,
                TextureFormat::BC5,
                TextureFormat::BC7,
            ]
        }

        #[cfg(not(feature = "texture-advanced"))]
        {
            vec![]
        }
    }
}

impl Default for CompressedDecoder {
    fn default() -> Self {
        Self::new()
    }
}
