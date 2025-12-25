//! Basic texture format decoders
//!
//! This module handles uncompressed texture formats like RGBA32, RGB24, etc.

use super::{Decoder, create_rgba_image, validate_dimensions};
use crate::error::{BinaryError, Result};
use crate::texture::formats::TextureFormat;
use crate::texture::types::Texture2D;
use image::RgbaImage;

/// Decoder for basic uncompressed texture formats
pub struct BasicDecoder;

impl BasicDecoder {
    /// Create a new basic decoder
    pub fn new() -> Self {
        Self
    }

    /// Decode RGBA32 format (R8G8B8A8)
    fn decode_rgba32(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let expected_size = (width * height * 4) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(format!(
                "Insufficient data for RGBA32: expected {}, got {}",
                expected_size,
                data.len()
            )));
        }

        // RGBA32 is already in the correct format
        create_rgba_image(data[..expected_size].to_vec(), width, height)
    }

    /// Decode RGB24 format (R8G8B8)
    fn decode_rgb24(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let expected_size = (width * height * 3) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(format!(
                "Insufficient data for RGB24: expected {}, got {}",
                expected_size,
                data.len()
            )));
        }

        // Convert RGB24 to RGBA32 by adding alpha channel
        let mut rgba_data = Vec::with_capacity((width * height * 4) as usize);
        for chunk in data[..expected_size].chunks_exact(3) {
            rgba_data.push(chunk[0]); // R
            rgba_data.push(chunk[1]); // G
            rgba_data.push(chunk[2]); // B
            rgba_data.push(255); // A (fully opaque)
        }

        create_rgba_image(rgba_data, width, height)
    }

    /// Decode ARGB32 format (A8R8G8B8)
    fn decode_argb32(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let expected_size = (width * height * 4) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(format!(
                "Insufficient data for ARGB32: expected {}, got {}",
                expected_size,
                data.len()
            )));
        }

        // Convert ARGB32 to RGBA32 by reordering channels
        let mut rgba_data = Vec::with_capacity(expected_size);
        for chunk in data[..expected_size].chunks_exact(4) {
            rgba_data.push(chunk[1]); // R (from position 1)
            rgba_data.push(chunk[2]); // G (from position 2)
            rgba_data.push(chunk[3]); // B (from position 3)
            rgba_data.push(chunk[0]); // A (from position 0)
        }

        create_rgba_image(rgba_data, width, height)
    }

    /// Decode BGRA32 format (B8G8R8A8)
    fn decode_bgra32(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let expected_size = (width * height * 4) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(format!(
                "Insufficient data for BGRA32: expected {}, got {}",
                expected_size,
                data.len()
            )));
        }

        // Convert BGRA32 to RGBA32 by swapping R and B channels
        let mut rgba_data = Vec::with_capacity(expected_size);
        for chunk in data[..expected_size].chunks_exact(4) {
            rgba_data.push(chunk[2]); // R (from B position)
            rgba_data.push(chunk[1]); // G (unchanged)
            rgba_data.push(chunk[0]); // B (from R position)
            rgba_data.push(chunk[3]); // A (unchanged)
        }

        create_rgba_image(rgba_data, width, height)
    }

    /// Decode Alpha8 format (single channel alpha)
    fn decode_alpha8(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let expected_size = (width * height) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(format!(
                "Insufficient data for Alpha8: expected {}, got {}",
                expected_size,
                data.len()
            )));
        }

        // Convert Alpha8 to RGBA32 (white with alpha)
        let mut rgba_data = Vec::with_capacity((width * height * 4) as usize);
        for &alpha in &data[..expected_size] {
            rgba_data.push(255); // R (white)
            rgba_data.push(255); // G (white)
            rgba_data.push(255); // B (white)
            rgba_data.push(alpha); // A (from source)
        }

        create_rgba_image(rgba_data, width, height)
    }

    /// Decode RGBA4444 format (4 bits per channel)
    fn decode_rgba4444(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let expected_size = (width * height * 2) as usize; // 2 bytes per pixel
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(format!(
                "Insufficient data for RGBA4444: expected {}, got {}",
                expected_size,
                data.len()
            )));
        }

        // Convert RGBA4444 to RGBA32
        let mut rgba_data = Vec::with_capacity((width * height * 4) as usize);
        for chunk in data[..expected_size].chunks_exact(2) {
            let pixel = u16::from_le_bytes([chunk[0], chunk[1]]);

            // Extract 4-bit channels and expand to 8-bit
            let r = ((pixel >> 12) & 0xF) as u8;
            let g = ((pixel >> 8) & 0xF) as u8;
            let b = ((pixel >> 4) & 0xF) as u8;
            let a = (pixel & 0xF) as u8;

            // Expand 4-bit to 8-bit by duplicating bits
            rgba_data.push(r << 4 | r);
            rgba_data.push(g << 4 | g);
            rgba_data.push(b << 4 | b);
            rgba_data.push(a << 4 | a);
        }

        create_rgba_image(rgba_data, width, height)
    }

    /// Decode ARGB4444 format (4 bits per channel, ARGB order)
    fn decode_argb4444(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let expected_size = (width * height * 2) as usize; // 2 bytes per pixel
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(format!(
                "Insufficient data for ARGB4444: expected {}, got {}",
                expected_size,
                data.len()
            )));
        }

        // Convert ARGB4444 to RGBA32
        let mut rgba_data = Vec::with_capacity((width * height * 4) as usize);
        for chunk in data[..expected_size].chunks_exact(2) {
            let pixel = u16::from_le_bytes([chunk[0], chunk[1]]);

            // Extract 4-bit channels (ARGB order)
            let a = ((pixel >> 12) & 0xF) as u8;
            let r = ((pixel >> 8) & 0xF) as u8;
            let g = ((pixel >> 4) & 0xF) as u8;
            let b = (pixel & 0xF) as u8;

            // Expand 4-bit to 8-bit by duplicating bits
            rgba_data.push(r << 4 | r); // R
            rgba_data.push(g << 4 | g); // G
            rgba_data.push(b << 4 | b); // B
            rgba_data.push(a << 4 | a); // A
        }

        create_rgba_image(rgba_data, width, height)
    }

    /// Decode RGB565 format (5-6-5 bits per channel)
    fn decode_rgb565(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        validate_dimensions(width, height)?;

        let expected_size = (width * height * 2) as usize; // 2 bytes per pixel
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(format!(
                "Insufficient data for RGB565: expected {}, got {}",
                expected_size,
                data.len()
            )));
        }

        // Convert RGB565 to RGBA32
        let mut rgba_data = Vec::with_capacity((width * height * 4) as usize);
        for chunk in data[..expected_size].chunks_exact(2) {
            let pixel = u16::from_le_bytes([chunk[0], chunk[1]]);

            // Extract RGB channels
            let r = ((pixel >> 11) & 0x1F) as u8;
            let g = ((pixel >> 5) & 0x3F) as u8;
            let b = (pixel & 0x1F) as u8;

            // Expand to 8-bit
            rgba_data.push((r << 3) | (r >> 2)); // 5-bit to 8-bit
            rgba_data.push((g << 2) | (g >> 4)); // 6-bit to 8-bit
            rgba_data.push((b << 3) | (b >> 2)); // 5-bit to 8-bit
            rgba_data.push(255); // Alpha (fully opaque)
        }

        create_rgba_image(rgba_data, width, height)
    }
}

impl Decoder for BasicDecoder {
    fn decode(&self, texture: &Texture2D) -> Result<RgbaImage> {
        let width = texture.width as u32;
        let height = texture.height as u32;
        let data = &texture.image_data;

        match texture.format {
            TextureFormat::RGBA32 => self.decode_rgba32(data, width, height),
            TextureFormat::RGB24 => self.decode_rgb24(data, width, height),
            TextureFormat::ARGB32 => self.decode_argb32(data, width, height),
            TextureFormat::BGRA32 => self.decode_bgra32(data, width, height),
            TextureFormat::Alpha8 => self.decode_alpha8(data, width, height),
            TextureFormat::RGBA4444 => self.decode_rgba4444(data, width, height),
            TextureFormat::ARGB4444 => self.decode_argb4444(data, width, height),
            TextureFormat::RGB565 => self.decode_rgb565(data, width, height),
            _ => Err(BinaryError::unsupported(format!(
                "Format {:?} is not a basic format",
                texture.format
            ))),
        }
    }

    fn can_decode(&self, format: TextureFormat) -> bool {
        format.is_basic_format()
    }

    fn supported_formats(&self) -> Vec<TextureFormat> {
        vec![
            TextureFormat::Alpha8,
            TextureFormat::RGB24,
            TextureFormat::RGBA32,
            TextureFormat::ARGB32,
            TextureFormat::BGRA32,
            TextureFormat::RGBA4444,
            TextureFormat::ARGB4444,
            TextureFormat::RGB565,
        ]
    }
}

impl Default for BasicDecoder {
    fn default() -> Self {
        Self::new()
    }
}
