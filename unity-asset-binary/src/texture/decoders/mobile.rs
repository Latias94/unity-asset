//! Mobile texture format decoders
//!
//! This module handles mobile-specific texture formats like ETC, ASTC, PVRTC, etc.
//! Requires the texture-advanced feature for texture2ddecoder integration.

use crate::error::{BinaryError, Result};
use super::{Decoder, create_rgba_image, validate_dimensions};
use crate::texture::formats::TextureFormat;
use crate::texture::types::Texture2D;
use image::RgbaImage;

/// Decoder for mobile texture formats
pub struct MobileDecoder;

impl MobileDecoder {
    /// Create a new mobile decoder
    pub fn new() -> Self {
        Self
    }

    /// Decode ETC2 RGB format
    #[cfg(feature = "texture-advanced")]
    fn decode_etc2_rgb(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;
        
        let mut output = vec![0u32; (width * height) as usize];
        
        match texture2ddecoder::decode_etc2_rgb(data, width as usize, height as usize, &mut output) {
            Ok(_) => {
                // Convert u32 RGBA to u8 RGBA
                let rgba_data: Vec<u8> = output
                    .iter()
                    .flat_map(|&pixel| {
                        [
                            (pixel & 0xFF) as u8,         // R
                            ((pixel >> 8) & 0xFF) as u8,  // G
                            ((pixel >> 16) & 0xFF) as u8, // B
                            255,                           // A (ETC2 RGB has no alpha)
                        ]
                    })
                    .collect();
                
                create_rgba_image(rgba_data, width, height)
            }
            Err(e) => Err(BinaryError::generic(format!("ETC2 RGB decoding failed: {}", e))),
        }
    }

    /// Decode ETC2 RGBA8 format
    #[cfg(feature = "texture-advanced")]
    fn decode_etc2_rgba8(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;
        
        let mut output = vec![0u32; (width * height) as usize];
        
        match texture2ddecoder::decode_etc2_rgba8(data, width as usize, height as usize, &mut output) {
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
            Err(e) => Err(BinaryError::generic(format!("ETC2 RGBA8 decoding failed: {}", e))),
        }
    }

    /// Decode ASTC 4x4 format
    #[cfg(feature = "texture-advanced")]
    fn decode_astc_4x4(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;
        
        let mut output = vec![0u32; (width * height) as usize];
        
        match texture2ddecoder::decode_astc(data, width as usize, height as usize, 4, 4, &mut output) {
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
            Err(e) => Err(BinaryError::generic(format!("ASTC 4x4 decoding failed: {}", e))),
        }
    }

    /// Decode ASTC 6x6 format
    #[cfg(feature = "texture-advanced")]
    fn decode_astc_6x6(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;
        
        let mut output = vec![0u32; (width * height) as usize];
        
        match texture2ddecoder::decode_astc(data, width as usize, height as usize, 6, 6, &mut output) {
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
            Err(e) => Err(BinaryError::generic(format!("ASTC 6x6 decoding failed: {}", e))),
        }
    }

    /// Decode ASTC 8x8 format
    #[cfg(feature = "texture-advanced")]
    fn decode_astc_8x8(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;
        
        let mut output = vec![0u32; (width * height) as usize];
        
        match texture2ddecoder::decode_astc(data, width as usize, height as usize, 8, 8, &mut output) {
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
            Err(e) => Err(BinaryError::generic(format!("ASTC 8x8 decoding failed: {}", e))),
        }
    }

    /// Fallback for when texture-advanced feature is not enabled
    #[cfg(not(feature = "texture-advanced"))]
    fn decode_unsupported(&self, format: TextureFormat) -> Result<RgbaImage> {
        Err(BinaryError::unsupported(format!(
            "Mobile format {:?} requires texture-advanced feature",
            format
        )))
    }
}

impl Decoder for MobileDecoder {
    fn decode(&self, texture: &Texture2D) -> Result<RgbaImage> {
        let width = texture.width as u32;
        let height = texture.height as u32;
        let data = &texture.image_data;

        match texture.format {
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGB => self.decode_etc2_rgb(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGBA8 => self.decode_etc2_rgba8(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_4x4 => self.decode_astc_4x4(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_6x6 => self.decode_astc_6x6(data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_8x8 => self.decode_astc_8x8(data, width, height),
            
            #[cfg(not(feature = "texture-advanced"))]
            format if format.is_mobile_format() => self.decode_unsupported(format),
            
            _ => Err(BinaryError::unsupported(format!(
                "Format {:?} is not a mobile format",
                texture.format
            ))),
        }
    }

    fn can_decode(&self, format: TextureFormat) -> bool {
        #[cfg(feature = "texture-advanced")]
        {
            matches!(
                format,
                TextureFormat::ETC2_RGB
                    | TextureFormat::ETC2_RGBA8
                    | TextureFormat::ASTC_RGBA_4x4
                    | TextureFormat::ASTC_RGBA_6x6
                    | TextureFormat::ASTC_RGBA_8x8
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
                TextureFormat::ETC2_RGB,
                TextureFormat::ETC2_RGBA8,
                TextureFormat::ASTC_RGBA_4x4,
                TextureFormat::ASTC_RGBA_6x6,
                TextureFormat::ASTC_RGBA_8x8,
            ]
        }
        
        #[cfg(not(feature = "texture-advanced"))]
        {
            vec![]
        }
    }
}

impl Default for MobileDecoder {
    fn default() -> Self {
        Self::new()
    }
}
