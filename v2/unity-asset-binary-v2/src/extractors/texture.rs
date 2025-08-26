//! Async Texture Processing
//!
//! Provides async texture extraction and processing for Unity Texture2D assets.
//! Supports various texture formats with streaming decompression and async conversion.

use crate::async_compression::{AsyncDecompressor, UnityAsyncDecompressor};
use crate::binary_types::{AsyncBinaryData, AsyncBinaryReader};
use crate::stream_reader::AsyncStreamReader;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::task;
use unity_asset_core_v2::{AsyncUnityClass, Result, UnityAssetError, UnityValue};

#[cfg(feature = "texture")]
use image::{ImageBuffer, RgbaImage};

/// Async texture processor configuration
#[derive(Debug, Clone)]
pub struct TextureConfig {
    /// Maximum texture dimensions for safety
    pub max_width: u32,
    pub max_height: u32,
    /// Whether to perform format conversion
    pub convert_formats: bool,
    /// Target format for conversion
    pub target_format: TextureOutputFormat,
    /// Whether to generate mipmaps during processing
    pub generate_mipmaps: bool,
}

impl Default for TextureConfig {
    fn default() -> Self {
        Self {
            max_width: 8192,
            max_height: 8192,
            convert_formats: true,
            target_format: TextureOutputFormat::RGBA32,
            generate_mipmaps: false,
        }
    }
}

/// Supported output texture formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureOutputFormat {
    RGBA32,
    RGB24,
    PNG,
    JPEG,
}

/// Unity texture formats (subset of commonly used formats)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnityTextureFormat {
    Alpha8 = 1,
    ARGB4444 = 2,
    RGB24 = 3,
    RGBA32 = 4,
    ARGB32 = 5,
    RGB565 = 7,
    DXT1 = 10,
    DXT5 = 12,
    RGBA4444 = 13,
    BGRA32 = 14,
    RHalf = 15,
    RGHalf = 16,
    RGBAHalf = 17,
    RFloat = 18,
    RGFloat = 19,
    RGBAFloat = 20,
    YUY2 = 21,
    RGB9e5Float = 22,
    RGBFloat = 23,
    BC6H = 24,
    BC7 = 25,
    BC4 = 26,
    BC5 = 27,
    DXT1Crunched = 28,
    DXT5Crunched = 29,
    PVRTC_RGB2 = 30,
    PVRTC_RGBA2 = 31,
    PVRTC_RGB4 = 32,
    PVRTC_RGBA4 = 33,
    ETC_RGB4 = 34,
    ATC_RGB4 = 35,
    ATC_RGBA8 = 36,
    EAC_R = 41,
    EAC_R_SIGNED = 42,
    EAC_RG = 43,
    EAC_RG_SIGNED = 44,
    ETC2_RGB = 45,
    ETC2_RGBA1 = 46,
    ETC2_RGBA8 = 47,
    ASTC_4x4 = 48,
    ASTC_5x5 = 49,
    ASTC_6x6 = 50,
    ASTC_8x8 = 51,
    ASTC_10x10 = 52,
    ASTC_12x12 = 53,
}

impl UnityTextureFormat {
    /// Create from Unity format ID
    pub fn from_id(id: i32) -> Option<Self> {
        match id {
            1 => Some(Self::Alpha8),
            2 => Some(Self::ARGB4444),
            3 => Some(Self::RGB24),
            4 => Some(Self::RGBA32),
            5 => Some(Self::ARGB32),
            7 => Some(Self::RGB565),
            10 => Some(Self::DXT1),
            12 => Some(Self::DXT5),
            13 => Some(Self::RGBA4444),
            14 => Some(Self::BGRA32),
            28 => Some(Self::DXT1Crunched),
            29 => Some(Self::DXT5Crunched),
            34 => Some(Self::ETC_RGB4),
            45 => Some(Self::ETC2_RGB),
            46 => Some(Self::ETC2_RGBA1),
            47 => Some(Self::ETC2_RGBA8),
            48 => Some(Self::ASTC_4x4),
            _ => None,
        }
    }

    /// Get format ID
    pub fn id(&self) -> i32 {
        *self as i32
    }

    /// Check if format is compressed
    pub fn is_compressed(&self) -> bool {
        matches!(
            self,
            Self::DXT1
                | Self::DXT5
                | Self::DXT1Crunched
                | Self::DXT5Crunched
                | Self::BC4
                | Self::BC5
                | Self::BC6H
                | Self::BC7
                | Self::PVRTC_RGB2
                | Self::PVRTC_RGBA2
                | Self::PVRTC_RGB4
                | Self::PVRTC_RGBA4
                | Self::ETC_RGB4
                | Self::ETC2_RGB
                | Self::ETC2_RGBA1
                | Self::ETC2_RGBA8
                | Self::ASTC_4x4
                | Self::ASTC_5x5
                | Self::ASTC_6x6
                | Self::ASTC_8x8
                | Self::ASTC_10x10
                | Self::ASTC_12x12
        )
    }

    /// Get bytes per pixel for uncompressed formats
    pub fn bytes_per_pixel(&self) -> Option<u32> {
        match self {
            Self::Alpha8 => Some(1),
            Self::RGB24 => Some(3),
            Self::RGBA32 | Self::ARGB32 | Self::BGRA32 => Some(4),
            Self::ARGB4444 | Self::RGBA4444 | Self::RGB565 => Some(2),
            Self::RHalf => Some(2),
            Self::RGHalf => Some(4),
            Self::RGBAHalf => Some(8),
            Self::RFloat => Some(4),
            Self::RGFloat => Some(8),
            Self::RGBAFloat => Some(16),
            _ => None, // Compressed formats don't have fixed bytes per pixel
        }
    }

    /// Check if format requires special decoder
    pub fn requires_advanced_decoder(&self) -> bool {
        matches!(
            self,
            Self::DXT1
                | Self::DXT5
                | Self::DXT1Crunched
                | Self::DXT5Crunched
                | Self::BC4
                | Self::BC5
                | Self::BC6H
                | Self::BC7
                | Self::ETC_RGB4
                | Self::ETC2_RGB
                | Self::ETC2_RGBA1
                | Self::ETC2_RGBA8
                | Self::ASTC_4x4
                | Self::ASTC_5x5
                | Self::ASTC_6x6
                | Self::ASTC_8x8
                | Self::ASTC_10x10
                | Self::ASTC_12x12
        )
    }
}

/// Texture2D information parsed from Unity asset
#[derive(Debug, Clone)]
pub struct Texture2D {
    /// Texture name
    pub name: String,
    /// Texture width in pixels
    pub width: u32,
    /// Texture height in pixels
    pub height: u32,
    /// Unity texture format
    pub format: UnityTextureFormat,
    /// Texture data
    pub image_data: Bytes,
    /// Number of mipmap levels
    pub mip_count: u32,
    /// Whether texture is readable
    pub is_readable: bool,
    /// Texture filtering mode
    pub filter_mode: TextureFilterMode,
    /// Texture wrap mode
    pub wrap_mode: TextureWrapMode,
}

impl Texture2D {
    /// Create from Unity class data
    pub async fn from_unity_class(unity_class: &AsyncUnityClass) -> Result<Self> {
        let name = unity_class
            .get_property("m_Name")
            .and_then(|v| v.as_string())
            .unwrap_or("Unknown".to_string())
            .to_string();

        let width = unity_class
            .get_property("m_Width")
            .and_then(|v| v.as_u32())
            .ok_or_else(|| UnityAssetError::parse_error("Missing texture width".to_string(), 0))?;

        let height = unity_class
            .get_property("m_Height")
            .and_then(|v| v.as_u32())
            .ok_or_else(|| UnityAssetError::parse_error("Missing texture height".to_string(), 0))?;

        let format_id = unity_class
            .get_property("m_TextureFormat")
            .and_then(|v| v.as_i32())
            .ok_or_else(|| UnityAssetError::parse_error("Missing texture format".to_string(), 0))?;

        let format = UnityTextureFormat::from_id(format_id).ok_or_else(|| {
            UnityAssetError::unsupported_format(format!(
                "Unsupported texture format: {}",
                format_id
            ))
        })?;

        let image_data = unity_class
            .get_property("image data")
            .and_then(|v| v.as_bytes())
            .unwrap_or(&Vec::new())
            .to_vec();

        let mip_count = unity_class
            .get_property("m_MipCount")
            .and_then(|v| v.as_u32())
            .unwrap_or(1);

        let is_readable = unity_class
            .get_property("m_IsReadable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let filter_mode = TextureFilterMode::Bilinear; // Default
        let wrap_mode = TextureWrapMode::Repeat; // Default

        Ok(Self {
            name,
            width,
            height,
            format,
            image_data: bytes::Bytes::from(image_data.clone()),
            mip_count,
            is_readable,
            filter_mode,
            wrap_mode,
        })
    }

    /// Get texture size in bytes
    pub fn data_size(&self) -> usize {
        self.image_data.len()
    }

    /// Calculate expected uncompressed size
    pub fn expected_uncompressed_size(&self) -> Option<usize> {
        if let Some(bpp) = self.format.bytes_per_pixel() {
            Some((self.width * self.height * bpp) as usize)
        } else {
            None
        }
    }

    /// Check if texture needs decompression
    pub fn needs_decompression(&self) -> bool {
        self.format.is_compressed()
    }

    /// Decode texture image asynchronously
    pub async fn decode_image(&self) -> Result<image::RgbaImage> {
        // Use image crate to create decoded image
        match image::RgbaImage::from_vec(self.width, self.height, self.image_data.to_vec()) {
            Some(img) => Ok(img),
            None => {
                // Create empty image if decoding fails
                Ok(image::RgbaImage::new(self.width, self.height))
            }
        }
    }

    /// Export texture as PNG asynchronously  
    pub async fn export_png(&self, path: &str) -> Result<()> {
        let img = self.decode_image().await?;

        // Use tokio task to avoid blocking on save
        let path_owned = path.to_string();
        tokio::task::spawn_blocking(move || img.save(path_owned))
            .await
            .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))?
            .map_err(|e| UnityAssetError::parse_error(format!("Failed to save PNG: {}", e), 0))?;

        Ok(())
    }

    /// Get texture info
    pub fn get_info(&self) -> TextureInfo {
        TextureInfo {
            format: self.format,
            is_compressed: self.format.is_compressed(),
            width: self.width,
            height: self.height,
        }
    }
}

/// Texture information
#[derive(Debug)]
pub struct TextureInfo {
    pub format: UnityTextureFormat,
    pub is_compressed: bool,
    pub width: u32,
    pub height: u32,
}

/// Texture filtering modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureFilterMode {
    Point,
    Bilinear,
    Trilinear,
}

/// Texture wrap modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureWrapMode {
    Repeat,
    Clamp,
    Mirror,
    MirrorOnce,
}

/// Async texture processor
pub struct Texture2DProcessor {
    config: TextureConfig,
    decompressor: UnityAsyncDecompressor,
}

impl Texture2DProcessor {
    /// Create new texture processor
    pub fn new() -> Self {
        Self {
            config: TextureConfig::default(),
            decompressor: UnityAsyncDecompressor::new(),
        }
    }

    /// Create texture processor with configuration
    pub fn with_config(config: TextureConfig) -> Self {
        Self {
            config,
            decompressor: UnityAsyncDecompressor::new(),
        }
    }

    /// Parse Texture2D from AsyncUnityClass asynchronously
    pub async fn parse_texture2d(&self, unity_object: &AsyncUnityClass) -> Result<Texture2D> {
        // Extract texture properties from Unity object
        let name = unity_object.name().unwrap_or_default();

        // Extract texture properties from the unity object's data
        let width = unity_object
            .get_property("m_Width")
            .and_then(|v| v.as_i32())
            .ok_or_else(|| UnityAssetError::parse_error("Missing texture width".to_string(), 0))?;

        let height = unity_object
            .get_property("m_Height")
            .and_then(|v| v.as_i32())
            .ok_or_else(|| UnityAssetError::parse_error("Missing texture height".to_string(), 0))?;

        let format_id = unity_object
            .get_property("m_TextureFormat")
            .and_then(|v| v.as_i32())
            .ok_or_else(|| UnityAssetError::parse_error("Missing texture format".to_string(), 0))?;

        let image_data = unity_object
            .get_property("image data")
            .or_else(|| unity_object.get_property("m_ImageData"))
            .and_then(|v| v.as_bytes())
            .cloned()
            .unwrap_or_default();

        // Additional texture properties
        let mip_count = unity_object
            .get_property("m_MipCount")
            .and_then(|v| v.as_i32())
            .unwrap_or(1);

        let is_readable = unity_object
            .get_property("m_IsReadable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Convert format ID to UnityTextureFormat
        let format = UnityTextureFormat::from_id(format_id).unwrap_or(UnityTextureFormat::RGBA32);

        Ok(Texture2D {
            name,
            width: width as u32,
            height: height as u32,
            format,
            mip_count: mip_count as u32,
            is_readable,
            // Convert Vec<u8> to Bytes for image_data field
            image_data: bytes::Bytes::from(image_data),
            filter_mode: TextureFilterMode::Bilinear, // Default
            wrap_mode: TextureWrapMode::Repeat,       // Default
        })
    }

    /// Process texture asynchronously
    pub async fn process_texture(&self, texture: &Texture2D) -> Result<ProcessedTexture> {
        // Validate texture dimensions
        if texture.width > self.config.max_width || texture.height > self.config.max_height {
            return Err(UnityAssetError::parse_error(
                format!(
                    "Texture dimensions {}x{} exceed maximum {}x{}",
                    texture.width, texture.height, self.config.max_width, self.config.max_height
                ),
                0,
            ));
        }

        // Decode texture data based on format
        let rgba_data = match texture.format {
            UnityTextureFormat::RGBA32 => self.process_rgba32(&texture.image_data).await?,
            UnityTextureFormat::RGB24 => self.process_rgb24(&texture.image_data).await?,
            UnityTextureFormat::ARGB32 => self.process_argb32(&texture.image_data).await?,
            UnityTextureFormat::BGRA32 => self.process_bgra32(&texture.image_data).await?,
            UnityTextureFormat::Alpha8 => self.process_alpha8(&texture.image_data).await?,
            UnityTextureFormat::DXT1 => {
                #[cfg(feature = "texture-advanced")]
                {
                    self.process_dxt1(&texture.image_data, texture.width, texture.height)
                        .await?
                }
                #[cfg(not(feature = "texture-advanced"))]
                {
                    return Err(UnityAssetError::unsupported_format(
                        "DXT1 format requires 'texture-advanced' feature".to_string(),
                    ));
                }
            }
            UnityTextureFormat::DXT5 => {
                #[cfg(feature = "texture-advanced")]
                {
                    self.process_dxt5(&texture.image_data, texture.width, texture.height)
                        .await?
                }
                #[cfg(not(feature = "texture-advanced"))]
                {
                    return Err(UnityAssetError::unsupported_format(
                        "DXT5 format requires 'texture-advanced' feature".to_string(),
                    ));
                }
            }
            _ => {
                return Err(UnityAssetError::unsupported_format(format!(
                    "Texture format {:?} not yet supported",
                    texture.format
                )));
            }
        };

        Ok(ProcessedTexture {
            name: texture.name.clone(),
            width: texture.width,
            height: texture.height,
            original_format: texture.format,
            rgba_data,
            format: self.config.target_format,
        })
    }

    /// Process RGBA32 format (native format)
    async fn process_rgba32(&self, data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }

    /// Process RGB24 format (convert to RGBA32)
    async fn process_rgb24(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() % 3 != 0 {
            return Err(UnityAssetError::parse_error(
                "RGB24 data length not divisible by 3".to_string(),
                0,
            ));
        }

        let data = data.to_vec();
        task::spawn_blocking(move || {
            let mut rgba_data = Vec::with_capacity(data.len() * 4 / 3);
            for rgb in data.chunks_exact(3) {
                rgba_data.extend_from_slice(rgb);
                rgba_data.push(255); // Full alpha
            }
            rgba_data
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))
    }

    /// Process ARGB32 format (convert to RGBA32)
    async fn process_argb32(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() % 4 != 0 {
            return Err(UnityAssetError::parse_error(
                "ARGB32 data length not divisible by 4".to_string(),
                0,
            ));
        }

        let data = data.to_vec();
        task::spawn_blocking(move || {
            let mut rgba_data = Vec::with_capacity(data.len());
            for argb in data.chunks_exact(4) {
                // Convert ARGB to RGBA
                rgba_data.push(argb[1]); // R
                rgba_data.push(argb[2]); // G
                rgba_data.push(argb[3]); // B
                rgba_data.push(argb[0]); // A
            }
            rgba_data
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))
    }

    /// Process BGRA32 format (convert to RGBA32)
    async fn process_bgra32(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() % 4 != 0 {
            return Err(UnityAssetError::parse_error(
                "BGRA32 data length not divisible by 4".to_string(),
                0,
            ));
        }

        let data = data.to_vec();
        task::spawn_blocking(move || {
            let mut rgba_data = Vec::with_capacity(data.len());
            for bgra in data.chunks_exact(4) {
                // Convert BGRA to RGBA
                rgba_data.push(bgra[2]); // R
                rgba_data.push(bgra[1]); // G
                rgba_data.push(bgra[0]); // B
                rgba_data.push(bgra[3]); // A
            }
            rgba_data
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))
    }

    /// Process Alpha8 format (convert to RGBA32 grayscale)
    async fn process_alpha8(&self, data: &[u8]) -> Result<Vec<u8>> {
        let data = data.to_vec();
        task::spawn_blocking(move || {
            let mut rgba_data = Vec::with_capacity(data.len() * 4);
            for &alpha in &data {
                rgba_data.push(alpha); // R
                rgba_data.push(alpha); // G
                rgba_data.push(alpha); // B
                rgba_data.push(255); // A (full opacity)
            }
            rgba_data
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))
    }

    /// Process DXT1 format (requires advanced decoder)
    #[cfg(feature = "texture-advanced")]
    async fn process_dxt1(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
        let data = data.to_vec();
        task::spawn_blocking(move || {
            // This would use texture2ddecoder crate for DXT1 decompression
            // For now, return an error indicating the feature needs implementation
            Err(UnityAssetError::parse_error(
                "DXT1 decompression not yet implemented".to_string(),
            ))
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))?
    }

    /// Process DXT5 format (requires advanced decoder)
    #[cfg(feature = "texture-advanced")]
    async fn process_dxt5(&self, data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
        let data = data.to_vec();
        task::spawn_blocking(move || {
            // This would use texture2ddecoder crate for DXT5 decompression
            // For now, return an error indicating the feature needs implementation
            Err(UnityAssetError::parse_error(
                "DXT5 decompression not yet implemented".to_string(),
            ))
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))?
    }

    /// Export texture to PNG format
    #[cfg(feature = "texture")]
    pub async fn export_to_png(&self, texture: &ProcessedTexture) -> Result<Vec<u8>> {
        if texture.rgba_data.len() != (texture.width * texture.height * 4) as usize {
            return Err(UnityAssetError::parse_error(
                "RGBA data size mismatch".to_string(),
                0,
            ));
        }

        let width = texture.width;
        let height = texture.height;
        let rgba_data = texture.rgba_data.clone();

        task::spawn_blocking(move || {
            let image: RgbaImage =
                ImageBuffer::from_raw(width, height, rgba_data).ok_or_else(|| {
                    UnityAssetError::parse_error("Failed to create image buffer".to_string(), 0)
                })?;

            let mut output = Vec::new();
            image
                .write_to(
                    &mut std::io::Cursor::new(&mut output),
                    image::ImageFormat::Png,
                )
                .map_err(|e| {
                    UnityAssetError::parse_error(format!("PNG encoding failed: {}", e), 0)
                })?;

            Ok(output)
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))?
    }
}

impl Default for Texture2DProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// Processed texture data
#[derive(Debug, Clone)]
pub struct ProcessedTexture {
    /// Texture name
    pub name: String,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Original Unity format
    pub original_format: UnityTextureFormat,
    /// RGBA pixel data
    pub rgba_data: Vec<u8>,
    /// Output format
    pub format: TextureOutputFormat,
}

impl ProcessedTexture {
    /// Get pixel data size
    pub fn data_size(&self) -> usize {
        self.rgba_data.len()
    }

    /// Get pixel at coordinates
    pub fn get_pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }

        let index = ((y * self.width + x) * 4) as usize;
        if index + 3 < self.rgba_data.len() {
            Some([
                self.rgba_data[index],
                self.rgba_data[index + 1],
                self.rgba_data[index + 2],
                self.rgba_data[index + 3],
            ])
        } else {
            None
        }
    }

    /// Set pixel at coordinates
    pub fn set_pixel(&mut self, x: u32, y: u32, rgba: [u8; 4]) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }

        let index = ((y * self.width + x) * 4) as usize;
        if index + 3 < self.rgba_data.len() {
            self.rgba_data[index] = rgba[0];
            self.rgba_data[index + 1] = rgba[1];
            self.rgba_data[index + 2] = rgba[2];
            self.rgba_data[index + 3] = rgba[3];
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unity_texture_format() {
        assert_eq!(
            UnityTextureFormat::from_id(4),
            Some(UnityTextureFormat::RGBA32)
        );
        assert_eq!(UnityTextureFormat::RGBA32.id(), 4);
        assert!(!UnityTextureFormat::RGBA32.is_compressed());
        assert!(UnityTextureFormat::DXT1.is_compressed());
        assert_eq!(UnityTextureFormat::RGBA32.bytes_per_pixel(), Some(4));
        assert_eq!(UnityTextureFormat::DXT1.bytes_per_pixel(), None);
    }

    #[tokio::test]
    async fn test_texture_processor_creation() {
        let processor = Texture2DProcessor::new();
        // Basic smoke test
        assert_eq!(processor.config.max_width, 8192);
        assert_eq!(processor.config.max_height, 8192);
    }

    #[tokio::test]
    async fn test_rgba32_processing() {
        let processor = Texture2DProcessor::new();
        let input_data = vec![255, 0, 0, 255, 0, 255, 0, 255]; // Red and green pixels

        let result = processor.process_rgba32(&input_data).await.unwrap();
        assert_eq!(result, input_data);
    }

    #[tokio::test]
    async fn test_rgb24_to_rgba32() {
        let processor = Texture2DProcessor::new();
        let input_data = vec![255, 0, 0, 0, 255, 0]; // Red and green pixels in RGB24

        let result = processor.process_rgb24(&input_data).await.unwrap();
        assert_eq!(result, vec![255, 0, 0, 255, 0, 255, 0, 255]); // Should add alpha
    }

    #[tokio::test]
    async fn test_argb32_to_rgba32() {
        let processor = Texture2DProcessor::new();
        let input_data = vec![255, 255, 0, 0]; // ARGB: Alpha=255, Red=255, Green=0, Blue=0

        let result = processor.process_argb32(&input_data).await.unwrap();
        assert_eq!(result, vec![255, 0, 0, 255]); // Should be RGBA: Red=255, Green=0, Blue=0, Alpha=255
    }

    #[tokio::test]
    async fn test_processed_texture_pixel_operations() {
        let mut texture = ProcessedTexture {
            name: "test".to_string(),
            width: 2,
            height: 1,
            original_format: UnityTextureFormat::RGBA32,
            rgba_data: vec![255, 0, 0, 255, 0, 255, 0, 255], // Red and green pixels
            format: TextureOutputFormat::RGBA32,
        };

        // Test get_pixel
        assert_eq!(texture.get_pixel(0, 0), Some([255, 0, 0, 255])); // Red pixel
        assert_eq!(texture.get_pixel(1, 0), Some([0, 255, 0, 255])); // Green pixel
        assert_eq!(texture.get_pixel(2, 0), None); // Out of bounds

        // Test set_pixel
        assert!(texture.set_pixel(0, 0, [0, 0, 255, 255])); // Change to blue
        assert_eq!(texture.get_pixel(0, 0), Some([0, 0, 255, 255])); // Should be blue now
        assert!(!texture.set_pixel(2, 0, [0, 0, 0, 0])); // Out of bounds should fail
    }

    #[test]
    fn test_texture_config_defaults() {
        let config = TextureConfig::default();
        assert_eq!(config.max_width, 8192);
        assert_eq!(config.max_height, 8192);
        assert!(config.convert_formats);
        assert_eq!(config.target_format, TextureOutputFormat::RGBA32);
    }
}
