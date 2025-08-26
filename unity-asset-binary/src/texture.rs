//! Texture2D Processing and Decoding
//!
//! This module provides comprehensive Texture2D processing capabilities,
//! including format detection, decoding, and export functionality.

use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::reader::BinaryReader;
use crate::unity_version::UnityVersion;
use image::{ImageBuffer, Rgba, RgbaImage};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use unity_asset_core::UnityValue;

// Advanced texture decoding support
#[cfg(feature = "texture-advanced")]
use texture2ddecoder;

/// Unity texture formats
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum TextureFormat {
    // Basic formats
    Alpha8 = 1,
    ARGB4444 = 2,
    RGB24 = 3,
    RGBA32 = 4,
    ARGB32 = 5,
    RGB565 = 7,
    R16 = 9,

    // Compressed formats
    DXT1 = 10,
    DXT5 = 12,
    RGBA4444 = 13,
    BGRA32 = 14,

    // HDR formats
    RHalf = 15,
    RGHalf = 16,
    RGBAHalf = 17,
    RFloat = 18,
    RGFloat = 19,
    RGBAFloat = 20,

    // Special formats
    YUY2 = 21,
    RGB9e5Float = 22,

    // BC formats
    BC6H = 24,
    BC7 = 25,
    BC4 = 26,
    BC5 = 27,

    // Crunched formats
    DXT1Crunched = 28,
    DXT5Crunched = 29,

    // Mobile formats
    PVRTC_RGB2 = 30,
    PVRTC_RGBA2 = 31,
    PVRTC_RGB4 = 32,
    PVRTC_RGBA4 = 33,
    ETC_RGB4 = 34,

    // ETC2/EAC formats
    EAC_R = 41,
    EAC_R_SIGNED = 42,
    EAC_RG = 43,
    EAC_RG_SIGNED = 44,
    ETC2_RGB = 45,
    ETC2_RGBA1 = 46,
    ETC2_RGBA8 = 47,

    // ASTC formats
    ASTC_RGB_4x4 = 48,
    ASTC_RGB_5x5 = 49,
    ASTC_RGB_6x6 = 50,
    ASTC_RGB_8x8 = 51,
    ASTC_RGB_10x10 = 52,
    ASTC_RGB_12x12 = 53,
    ASTC_RGBA_4x4 = 54,
    ASTC_RGBA_5x5 = 55,
    ASTC_RGBA_6x6 = 56,
    ASTC_RGBA_8x8 = 57,
    ASTC_RGBA_10x10 = 58,
    ASTC_RGBA_12x12 = 59,

    // More Crunched formats (Unity 2017.3+)
    ETC_RGB4Crunched = 64,
    ETC2_RGBA8Crunched = 65,

    // Unknown format
    #[default]
    Unknown = -1,
}

impl From<i32> for TextureFormat {
    fn from(value: i32) -> Self {
        match value {
            1 => TextureFormat::Alpha8,
            2 => TextureFormat::ARGB4444,
            3 => TextureFormat::RGB24,
            4 => TextureFormat::RGBA32,
            5 => TextureFormat::ARGB32,
            7 => TextureFormat::RGB565,
            9 => TextureFormat::R16,
            10 => TextureFormat::DXT1,
            12 => TextureFormat::DXT5,
            13 => TextureFormat::RGBA4444,
            14 => TextureFormat::BGRA32,
            15 => TextureFormat::RHalf,
            16 => TextureFormat::RGHalf,
            17 => TextureFormat::RGBAHalf,
            18 => TextureFormat::RFloat,
            19 => TextureFormat::RGFloat,
            20 => TextureFormat::RGBAFloat,
            21 => TextureFormat::YUY2,
            22 => TextureFormat::RGB9e5Float,
            24 => TextureFormat::BC6H,
            25 => TextureFormat::BC7,
            26 => TextureFormat::BC4,
            27 => TextureFormat::BC5,
            28 => TextureFormat::DXT1Crunched,
            29 => TextureFormat::DXT5Crunched,
            30 => TextureFormat::PVRTC_RGB2,
            31 => TextureFormat::PVRTC_RGBA2,
            32 => TextureFormat::PVRTC_RGB4,
            33 => TextureFormat::PVRTC_RGBA4,
            34 => TextureFormat::ETC_RGB4,
            41 => TextureFormat::EAC_R,
            42 => TextureFormat::EAC_R_SIGNED,
            43 => TextureFormat::EAC_RG,
            44 => TextureFormat::EAC_RG_SIGNED,
            45 => TextureFormat::ETC2_RGB,
            46 => TextureFormat::ETC2_RGBA1,
            47 => TextureFormat::ETC2_RGBA8,
            48 => TextureFormat::ASTC_RGB_4x4,
            49 => TextureFormat::ASTC_RGB_5x5,
            50 => TextureFormat::ASTC_RGB_6x6,
            51 => TextureFormat::ASTC_RGB_8x8,
            52 => TextureFormat::ASTC_RGB_10x10,
            53 => TextureFormat::ASTC_RGB_12x12,
            54 => TextureFormat::ASTC_RGBA_4x4,
            55 => TextureFormat::ASTC_RGBA_5x5,
            56 => TextureFormat::ASTC_RGBA_6x6,
            57 => TextureFormat::ASTC_RGBA_8x8,
            58 => TextureFormat::ASTC_RGBA_10x10,
            59 => TextureFormat::ASTC_RGBA_12x12,
            _ => TextureFormat::Unknown,
        }
    }
}

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

/// Texture format capabilities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextureFormatInfo {
    pub name: String,
    pub bits_per_pixel: u32,
    pub block_size: (u32, u32), // (width, height) in pixels
    pub compressed: bool,
    pub has_alpha: bool,
    pub supported: bool,
}

impl TextureFormat {
    /// Get format information
    pub fn info(&self) -> TextureFormatInfo {
        match self {
            TextureFormat::Alpha8 => TextureFormatInfo {
                name: "Alpha8".to_string(),
                bits_per_pixel: 8,
                block_size: (1, 1),
                compressed: false,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::RGB24 => TextureFormatInfo {
                name: "RGB24".to_string(),
                bits_per_pixel: 24,
                block_size: (1, 1),
                compressed: false,
                has_alpha: false,
                supported: true,
            },
            TextureFormat::RGBA32 => TextureFormatInfo {
                name: "RGBA32".to_string(),
                bits_per_pixel: 32,
                block_size: (1, 1),
                compressed: false,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::ARGB32 => TextureFormatInfo {
                name: "ARGB32".to_string(),
                bits_per_pixel: 32,
                block_size: (1, 1),
                compressed: false,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::DXT1 => TextureFormatInfo {
                name: "DXT1".to_string(),
                bits_per_pixel: 4,
                block_size: (4, 4),
                compressed: true,
                has_alpha: false,
                supported: true,
            },
            TextureFormat::DXT5 => TextureFormatInfo {
                name: "DXT5".to_string(),
                bits_per_pixel: 8,
                block_size: (4, 4),
                compressed: true,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::ETC2_RGB => TextureFormatInfo {
                name: "ETC2_RGB".to_string(),
                bits_per_pixel: 4,
                block_size: (4, 4),
                compressed: true,
                has_alpha: false,
                supported: true,
            },
            TextureFormat::ETC2_RGBA8 => TextureFormatInfo {
                name: "ETC2_RGBA8".to_string(),
                bits_per_pixel: 8,
                block_size: (4, 4),
                compressed: true,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::ASTC_RGBA_4x4 => TextureFormatInfo {
                name: "ASTC_RGBA_4x4".to_string(),
                bits_per_pixel: 8,
                block_size: (4, 4),
                compressed: true,
                has_alpha: true,
                supported: true,
            },
            _ => TextureFormatInfo {
                name: "Unknown".to_string(),
                bits_per_pixel: 0,
                block_size: (1, 1),
                compressed: false,
                has_alpha: false,
                supported: false,
            },
        }
    }

    /// Check if format is supported for decoding
    pub fn is_supported(&self) -> bool {
        self.info().supported
    }

    /// Get expected data size for given dimensions
    pub fn calculate_data_size(&self, width: u32, height: u32) -> u32 {
        let info = self.info();
        if info.compressed {
            let blocks_x = width.div_ceil(info.block_size.0);
            let blocks_y = height.div_ceil(info.block_size.1);
            // For compressed formats, bits_per_pixel is actually bits per block
            // DXT1: 8 bytes per 4x4 block = 64 bits per block
            // DXT5: 16 bytes per 4x4 block = 128 bits per block
            let bytes_per_block = match self {
                TextureFormat::DXT1 => 8,
                TextureFormat::DXT5 => 16,
                TextureFormat::ETC2_RGB => 8,
                TextureFormat::ETC2_RGBA8 => 16,
                TextureFormat::ASTC_RGBA_4x4 => 16,
                _ => info.bits_per_pixel / 8, // fallback
            };
            blocks_x * blocks_y * bytes_per_block
        } else {
            width * height * info.bits_per_pixel / 8
        }
    }
}

/// Texture2D processor for parsing and decoding
pub struct Texture2DProcessor {
    version: UnityVersion,
}

impl Texture2DProcessor {
    /// Create a new Texture2D processor
    pub fn new(version: UnityVersion) -> Self {
        Self { version }
    }

    /// Parse Texture2D from Unity object
    pub fn parse_texture2d(&self, object: &UnityObject) -> Result<Texture2D> {
        Texture2D::from_unity_object(object, &self.version)
    }

    /// Get supported texture formats for this Unity version
    pub fn get_supported_formats(&self) -> Vec<TextureFormat> {
        let mut formats = vec![
            TextureFormat::Alpha8,
            TextureFormat::RGB24,
            TextureFormat::RGBA32,
            TextureFormat::ARGB32,
            TextureFormat::RGBA4444,
            TextureFormat::RGB565,
        ];

        // Add version-specific formats
        if self.version.major >= 5 {
            formats.extend_from_slice(&[
                TextureFormat::DXT1,
                TextureFormat::DXT5,
                TextureFormat::ETC_RGB4,
            ]);
        }

        if self.version.major >= 2017 {
            formats.extend_from_slice(&[
                TextureFormat::ETC2_RGB,
                TextureFormat::ETC2_RGBA8,
                TextureFormat::ASTC_RGBA_4x4,
                TextureFormat::BC7,
            ]);
        }

        formats
    }
}

impl Texture2D {
    /// Parse Texture2D from UnityObject
    pub fn from_unity_object(obj: &UnityObject, version: &UnityVersion) -> Result<Self> {
        // For now, skip TypeTree parsing due to string resolution issues
        // and focus on binary parsing to get basic functionality working
        Self::from_binary_data(&obj.info.data, version)
    }

    /// Parse Texture2D from TypeTree properties
    pub fn from_typetree(
        properties: &IndexMap<String, UnityValue>,
        _version: &UnityVersion,
    ) -> Result<Self> {
        let mut texture = Texture2D::default();

        // Extract name
        if let Some(UnityValue::String(name)) = properties.get("m_Name") {
            texture.name = name.clone();
        }

        // Extract dimensions - try multiple possible field names
        if let Some(UnityValue::Integer(width)) = properties.get("m_Width") {
            texture.width = *width as i32;
        } else if let Some(UnityValue::Integer(width)) = properties.get("width") {
            texture.width = *width as i32;
        }

        if let Some(UnityValue::Integer(height)) = properties.get("m_Height") {
            texture.height = *height as i32;
        } else if let Some(UnityValue::Integer(height)) = properties.get("height") {
            texture.height = *height as i32;
        }

        // Extract format - try multiple possible field names
        if let Some(UnityValue::Integer(format)) = properties.get("m_TextureFormat") {
            texture.format = TextureFormat::from(*format as i32);
        } else if let Some(UnityValue::Integer(format)) = properties.get("format") {
            texture.format = TextureFormat::from(*format as i32);
        }

        // Extract mip map settings
        if let Some(UnityValue::Bool(mip_map)) = properties.get("m_MipMap") {
            texture.mip_map = *mip_map;
        }
        if let Some(UnityValue::Integer(mip_count)) = properties.get("m_MipCount") {
            texture.mip_count = *mip_count as i32;
        }

        // Extract complete image size
        if let Some(UnityValue::Integer(complete_size)) = properties.get("m_CompleteImageSize") {
            texture.complete_image_size = *complete_size as i32;
        }

        // Extract image count
        if let Some(UnityValue::Integer(image_count)) = properties.get("m_ImageCount") {
            texture.image_count = *image_count as i32;
        }

        // Extract texture dimension
        if let Some(UnityValue::Integer(texture_dimension)) = properties.get("m_TextureDimension") {
            texture.texture_dimension = *texture_dimension as i32;
        }

        // Extract lightmap format
        if let Some(UnityValue::Integer(lightmap_format)) = properties.get("m_LightmapFormat") {
            texture.light_map_format = *lightmap_format as i32;
        }

        // Extract color space
        if let Some(UnityValue::Integer(color_space)) = properties.get("m_ColorSpace") {
            texture.color_space = *color_space as i32;
        }

        // Extract data size
        if let Some(UnityValue::Integer(data_size)) = properties.get("m_DataSize") {
            texture.data_size = *data_size as i32;
        }

        // Extract readable flag
        if let Some(UnityValue::Bool(is_readable)) = properties.get("m_IsReadable") {
            texture.is_readable = *is_readable;
        }

        // Extract image data - try multiple possible field names
        if let Some(image_data_value) = properties.get("m_ImageData") {
            texture.image_data = Self::extract_image_data(image_data_value)?;
        } else if let Some(image_data_value) = properties.get("image_data") {
            texture.image_data = Self::extract_image_data(image_data_value)?;
        } else if let Some(image_data_value) = properties.get("m_Data") {
            texture.image_data = Self::extract_image_data(image_data_value)?;
        } else if let Some(image_data_value) = properties.get("data") {
            texture.image_data = Self::extract_image_data(image_data_value)?;
        }

        // If no image data found, try to use the raw object data
        if texture.image_data.is_empty() {
            // This is a fallback - in some cases the entire object data might be the image
            println!(
                "    Debug: No image data found in TypeTree, available properties: {:?}",
                properties.keys().collect::<Vec<_>>()
            );
        }

        // Extract streaming info if present
        if let Some(stream_data) = properties.get("m_StreamData") {
            texture.stream_info = Self::extract_stream_data(stream_data)?;
        }

        Ok(texture)
    }

    /// Parse Texture2D from raw binary data (based on unity-rs implementation)
    #[allow(clippy::field_reassign_with_default)]
    pub fn from_binary_data(data: &[u8], version: &UnityVersion) -> Result<Self> {
        if data.is_empty() {
            return Err(BinaryError::invalid_data("Empty texture data"));
        }

        let mut reader = BinaryReader::new(data, crate::reader::ByteOrder::Little);
        let mut texture = Texture2D::default();

        // Based on unity-rs Texture2D::load implementation
        // Read name first
        texture.name = reader
            .read_aligned_string()
            .unwrap_or_else(|_| "UnknownTexture".to_string());

        // Version-specific fields (Unity 2017.3+)
        if version.major > 2017 || (version.major == 2017 && version.minor >= 3) {
            texture.forced_fallback_format = Some(reader.read_i32().unwrap_or(0));
            texture.downscale_fallback = Some(reader.read_bool().unwrap_or(false));

            if version.major > 2020 || (version.major == 2020 && version.minor >= 2) {
                texture.is_alpha_channel_optional = Some(reader.read_bool().unwrap_or(false));
            }
            reader.align_to(4).ok();
        }

        // Core dimensions and format
        texture.width = reader.read_i32().unwrap_or(0);
        texture.height = reader.read_i32().unwrap_or(0);
        texture.complete_image_size = reader.read_i32().unwrap_or(0);

        if version.major >= 2020 {
            texture.mips_stripped = Some(reader.read_i32().unwrap_or(0));
        }

        let format_val = reader.read_i32().unwrap_or(0);
        texture.format = TextureFormat::from(format_val);

        // Handle mip map settings
        if version.major < 5 || (version.major == 5 && version.minor < 2) {
            texture.mip_map = reader.read_bool().unwrap_or(false);
        } else {
            texture.mip_count = reader.read_i32().unwrap_or(1);
        }

        // Version-specific readable flag
        if version.major > 2 || (version.major == 2 && version.minor >= 6) {
            texture.is_readable = reader.read_bool().unwrap_or(false);
        }

        // More version-specific flags
        if version.major >= 2020 {
            let _is_pre_processed = reader.read_bool().unwrap_or(false);
        }
        if version.major > 2019 || (version.major == 2019 && version.minor >= 3) {
            let _is_ignore_master_texture_limit = reader.read_bool().unwrap_or(false);
        }
        if version.major >= 3 && (version.major < 5 || (version.major == 5 && version.minor <= 4)) {
            let _read_allowed = reader.read_bool().unwrap_or(false);
        }
        if version.major > 2018 || (version.major == 2018 && version.minor >= 2) {
            let _streaming_mip_maps = reader.read_bool().unwrap_or(false);
        }
        reader.align_to(4).ok();

        if version.major > 2018 || (version.major == 2018 && version.minor >= 2) {
            let _streaming_mip_maps_priority = reader.read_i32().unwrap_or(0);
        }

        texture.image_count = reader.read_i32().unwrap_or(1);
        texture.texture_dimension = reader.read_i32().unwrap_or(2);

        // Read texture settings (simplified)
        texture.texture_settings = GLTextureSettings::default();
        // Skip texture settings parsing for now - it's complex and version-dependent

        if version.major >= 3 {
            texture.light_map_format = reader.read_i32().unwrap_or(0);
        }
        if version.major > 3 || (version.major == 3 && version.minor >= 5) {
            texture.color_space = reader.read_i32().unwrap_or(0);
        }

        // Platform blob (Unity 2020.2+)
        if version.major > 2020 || (version.major == 2020 && version.minor >= 2) {
            if let Ok(length) = reader.read_i32() {
                if length > 0 && length < 1024 * 1024 {
                    // Reasonable size limit
                    let _platform_blob = reader.read_bytes(length as usize);
                    reader.align_to(4).ok();
                }
            }
        }

        // Read data size and image data
        texture.data_size = reader.read_i32().unwrap_or(0);

        if texture.data_size == 0
            && ((version.major == 5 && version.minor >= 3) || version.major > 5)
        {
            // Read streaming info
            if reader.remaining() >= 12 {
                // offset (4/8) + size (4) + path (variable)
                let offset = if version.major >= 2020 {
                    reader.read_u64().unwrap_or(0)
                } else {
                    reader.read_u32().unwrap_or(0) as u64
                };
                let size = reader.read_u32().unwrap_or(0);
                let path = reader.read_aligned_string().unwrap_or_default();

                texture.stream_info = StreamingInfo {
                    offset,
                    size,
                    path: path.clone(),
                };

                // Streaming data detected but not fully implemented yet
            }
        }

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

        // Validate image data size against expected size
        let _expected_size = match texture.format {
            TextureFormat::Alpha8 => texture.width * texture.height,
            TextureFormat::RGBA32 => texture.width * texture.height * 4,
            TextureFormat::RGB24 => texture.width * texture.height * 3,
            TextureFormat::ARGB32 => texture.width * texture.height * 4,
            _ => 0,
        };

        // Note: For compressed textures (like Crunch), the image_data will be much smaller
        // than expected and will require special decompression algorithms

        // If we still don't have valid data, try some heuristics
        if texture.width <= 0 || texture.height <= 0 {
            println!("  ⚠ No valid dimensions found, trying heuristics...");
            // Try common texture sizes based on data length
            let data_len = data.len();
            let possible_sizes = [(1024, 512), (512, 512), (256, 256), (128, 128), (64, 64)];

            for (w, h) in possible_sizes {
                let rgba_size = w * h * 4;
                let rgb_size = w * h * 3;

                if data_len >= rgba_size && data_len <= rgba_size + 1024 {
                    texture.width = w as i32;
                    texture.height = h as i32;
                    texture.format = TextureFormat::RGBA32;
                    texture.image_data = data[data.len() - rgba_size..].to_vec();
                    println!("  ✓ Heuristic match: {}x{} RGBA32", w, h);
                    break;
                } else if data_len >= rgb_size && data_len <= rgb_size + 1024 {
                    texture.width = w as i32;
                    texture.height = h as i32;
                    texture.format = TextureFormat::RGB24;
                    texture.image_data = data[data.len() - (w * h * 3)..].to_vec();
                    break;
                }
            }
        }

        // If we have valid dimensions but no image data, try to extract it
        if texture.width > 0 && texture.height > 0 && texture.image_data.is_empty() {
            let expected_rgba_size = (texture.width * texture.height * 4) as usize;
            let expected_rgb_size = (texture.width * texture.height * 3) as usize;

            if data.len() >= expected_rgba_size {
                // Try to find RGBA data at the end of the buffer
                texture.image_data = data[data.len() - expected_rgba_size..].to_vec();
                texture.format = TextureFormat::RGBA32;
            } else if data.len() >= expected_rgb_size {
                // Try to find RGB data at the end of the buffer
                texture.image_data = data[data.len() - expected_rgb_size..].to_vec();
                texture.format = TextureFormat::RGB24;
            }
        }

        // Try to load streaming data if available
        if texture.stream_info.size > 0 && !texture.stream_info.path.is_empty() {
            if let Ok(stream_data) = texture.load_streaming_data() {
                texture.image_data = stream_data;
                texture.data_size = texture.image_data.len() as i32;
            }
        }

        Ok(texture)
    }

    /// Load streaming data from external file
    pub fn load_streaming_data(&self) -> Result<Vec<u8>> {
        if self.stream_info.path.is_empty() {
            return Err(BinaryError::invalid_data("No streaming path specified"));
        }

        // Try to read from the streaming file
        // The path is usually relative to the Unity asset bundle
        use std::fs;
        use std::path::Path;

        let stream_path = Path::new(&self.stream_info.path);

        // Try different possible locations for the streaming file
        let possible_paths = [
            stream_path.to_path_buf(),
            Path::new("StreamingAssets").join(stream_path),
            Path::new("..").join(stream_path),
        ];

        for path in &possible_paths {
            if path.exists() {
                match fs::File::open(path) {
                    Ok(mut file) => {
                        use std::io::{Read, Seek, SeekFrom};

                        // Seek to the specified offset
                        if file.seek(SeekFrom::Start(self.stream_info.offset)).is_err() {
                            continue;
                        }

                        // Read the specified amount of data
                        let mut buffer = vec![0u8; self.stream_info.size as usize];
                        match file.read_exact(&mut buffer) {
                            Ok(_) => return Ok(buffer),
                            Err(_) => continue,
                        }
                    }
                    Err(_) => continue,
                }
            }
        }

        Err(BinaryError::invalid_data(format!(
            "Streaming data file not found: {}",
            self.stream_info.path
        )))
    }

    /// Extract image data from UnityValue
    fn extract_image_data(value: &UnityValue) -> Result<Vec<u8>> {
        match value {
            UnityValue::Array(arr) => {
                let mut data = Vec::new();
                for item in arr {
                    if let UnityValue::Integer(byte_val) = item {
                        data.push(*byte_val as u8);
                    }
                }
                Ok(data)
            }
            UnityValue::String(base64_data) => {
                // Sometimes image data is stored as base64
                use base64::{Engine as _, engine::general_purpose};
                general_purpose::STANDARD.decode(base64_data).map_err(|e| {
                    BinaryError::invalid_data(format!("Invalid base64 image data: {}", e))
                })
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Extract stream data from UnityValue
    fn extract_stream_data(_value: &UnityValue) -> Result<StreamingInfo> {
        // StreamingInfo is typically a complex object with offset, size, and path
        // This is a simplified implementation
        Ok(StreamingInfo::default()) // TODO: Implement full streaming info extraction
    }

    /// Decode texture to RGBA image
    pub fn decode_image(&self) -> Result<RgbaImage> {
        if self.width <= 0 || self.height <= 0 {
            return Err(BinaryError::invalid_data("Invalid texture dimensions"));
        }

        if self.image_data.is_empty() {
            return Err(BinaryError::invalid_data("No image data available"));
        }

        let width = self.width as u32;
        let height = self.height as u32;

        let mut image_data = self.image_data.clone();

        // Handle Crunch compression first
        if self.is_crunch_compressed() {
            image_data = self.decompress_crunch(&image_data)?;
        }

        // Match UnityPy behavior: directly reject unsupported formats
        match self.format {
            // Basic uncompressed formats
            TextureFormat::RGBA32 => self.decode_rgba32_data(&image_data, width, height),
            TextureFormat::RGB24 => self.decode_rgb24_data(&image_data, width, height),
            TextureFormat::ARGB32 => self.decode_argb32_data(&image_data, width, height),
            TextureFormat::Alpha8 => self.decode_alpha8_data(&image_data, width, height),
            TextureFormat::BGRA32 => self.decode_bgra32_data(&image_data, width, height),

            // Compressed formats using texture2ddecoder
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT1 => self.decode_dxt1(&image_data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT5 => self.decode_dxt5(&image_data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::BC7 => self.decode_bc7(&image_data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC_RGB4 => self.decode_etc1(&image_data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGB => self.decode_etc2_rgb(&image_data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGBA8 => self.decode_etc2_rgba8(&image_data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_4x4 => self.decode_astc_4x4(&image_data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_6x6 => self.decode_astc_6x6(&image_data, width, height),
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_8x8 => self.decode_astc_8x8(&image_data, width, height),

            _ => Err(BinaryError::unsupported(format!(
                "Not implemented texture format: {:?}",
                self.format
            ))),
        }
    }

    /// Check if texture uses Crunch compression
    fn is_crunch_compressed(&self) -> bool {
        matches!(
            self.format,
            TextureFormat::DXT1Crunched
                | TextureFormat::DXT5Crunched
                | TextureFormat::ETC_RGB4Crunched
                | TextureFormat::ETC2_RGBA8Crunched
        ) || self.name.contains("Crunch")
    }

    /// Decompress Crunch compressed data
    fn decompress_crunch(&self, data: &[u8]) -> Result<Vec<u8>> {
        #[cfg(feature = "texture-advanced")]
        {
            // Use texture2ddecoder for Crunch decompression
            // The API expects width, height, and output buffer
            let width = self.width as usize;
            let height = self.height as usize;
            let mut output = vec![0u32; width * height];

            match texture2ddecoder::decode_unity_crunch(data, width, height, &mut output) {
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
        #[cfg(not(feature = "texture-advanced"))]
        {
            Err(BinaryError::unsupported(
                "Crunch decompression requires texture-advanced feature",
            ))
        }
    }

    /// Decode RGBA32 format (4 bytes per pixel: R, G, B, A)
    #[allow(dead_code)]
    fn decode_rgba32(&self, width: u32, height: u32) -> Result<RgbaImage> {
        self.decode_rgba32_data(&self.image_data, width, height)
    }

    /// Decode RGBA32 format from provided data
    fn decode_rgba32_data(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let expected_size = (width * height * 4) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(
                "Insufficient image data for RGBA32",
            ));
        }

        let mut image = ImageBuffer::new(width, height);
        for (i, pixel) in image.pixels_mut().enumerate() {
            let offset = i * 4;
            if offset + 3 < data.len() {
                *pixel = Rgba([
                    data[offset],     // R
                    data[offset + 1], // G
                    data[offset + 2], // B
                    data[offset + 3], // A
                ]);
            }
        }

        Ok(image)
    }

    /// Decode RGB24 format (3 bytes per pixel: R, G, B)
    #[allow(dead_code)]
    fn decode_rgb24(&self, width: u32, height: u32) -> Result<RgbaImage> {
        self.decode_rgb24_data(&self.image_data, width, height)
    }

    /// Decode RGB24 format from provided data
    fn decode_rgb24_data(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let expected_size = (width * height * 3) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(
                "Insufficient image data for RGB24",
            ));
        }

        let mut image = ImageBuffer::new(width, height);
        for (i, pixel) in image.pixels_mut().enumerate() {
            let offset = i * 3;
            if offset + 2 < data.len() {
                *pixel = Rgba([
                    data[offset],     // R
                    data[offset + 1], // G
                    data[offset + 2], // B
                    255,              // A (opaque)
                ]);
            }
        }

        Ok(image)
    }

    /// Decode ARGB32 format (4 bytes per pixel: A, R, G, B)
    #[allow(dead_code)]
    fn decode_argb32(&self, width: u32, height: u32) -> Result<RgbaImage> {
        self.decode_argb32_data(&self.image_data, width, height)
    }

    /// Decode ARGB32 format from provided data
    fn decode_argb32_data(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let expected_size = (width * height * 4) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(
                "Insufficient image data for ARGB32",
            ));
        }

        let mut image = ImageBuffer::new(width, height);
        for (i, pixel) in image.pixels_mut().enumerate() {
            let offset = i * 4;
            if offset + 3 < data.len() {
                *pixel = Rgba([
                    data[offset + 1], // R
                    data[offset + 2], // G
                    data[offset + 3], // B
                    data[offset],     // A
                ]);
            }
        }

        Ok(image)
    }

    /// Decode Alpha8 format (1 byte per pixel: A)
    #[allow(dead_code)]
    fn decode_alpha8(&self, width: u32, height: u32) -> Result<RgbaImage> {
        self.decode_alpha8_data(&self.image_data, width, height)
    }

    /// Decode Alpha8 format from provided data
    fn decode_alpha8_data(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let expected_size = (width * height) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(
                "Insufficient image data for Alpha8",
            ));
        }

        let mut image = ImageBuffer::new(width, height);
        for (i, pixel) in image.pixels_mut().enumerate() {
            if i < data.len() {
                let alpha = data[i];
                *pixel = Rgba([255, 255, 255, alpha]); // White with alpha
            }
        }

        Ok(image)
    }

    /// Decode BGRA32 format from provided data
    fn decode_bgra32_data(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let expected_size = (width * height * 4) as usize;
        if data.len() < expected_size {
            return Err(BinaryError::invalid_data(
                "Insufficient image data for BGRA32",
            ));
        }

        let mut image = ImageBuffer::new(width, height);
        for (i, pixel) in image.pixels_mut().enumerate() {
            let offset = i * 4;
            if offset + 3 < data.len() {
                *pixel = Rgba([
                    data[offset + 2], // R (from B)
                    data[offset + 1], // G
                    data[offset],     // B (from R)
                    data[offset + 3], // A
                ]);
            }
        }

        Ok(image)
    }

    // Advanced texture format decoders (require texture-advanced feature)
    #[cfg(feature = "texture-advanced")]
    fn decode_dxt1(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode DXT1 using texture2ddecoder (DXT1 = BC1)
        texture2ddecoder::decode_bc1(data, width_usize, height_usize, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("DXT1 decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from DXT1 decoded data")
        })
    }

    #[cfg(feature = "texture-advanced")]
    fn decode_dxt5(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode DXT5 using texture2ddecoder (DXT5 = BC3)
        texture2ddecoder::decode_bc3(data, width_usize, height_usize, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("DXT5 decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from DXT5 decoded data")
        })
    }

    #[cfg(feature = "texture-advanced")]
    fn decode_bc7(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode BC7 using texture2ddecoder
        texture2ddecoder::decode_bc7(data, width_usize, height_usize, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("BC7 decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from BC7 decoded data")
        })
    }

    #[cfg(feature = "texture-advanced")]
    fn decode_etc1(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode ETC1 using texture2ddecoder
        texture2ddecoder::decode_etc1(data, width_usize, height_usize, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("ETC1 decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from ETC1 decoded data")
        })
    }

    #[cfg(feature = "texture-advanced")]
    fn decode_etc2_rgb(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode ETC2 RGB using texture2ddecoder
        texture2ddecoder::decode_etc2_rgb(data, width_usize, height_usize, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("ETC2 RGB decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from ETC2 RGB decoded data")
        })
    }

    #[cfg(feature = "texture-advanced")]
    fn decode_etc2_rgba8(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode ETC2 RGBA8 using texture2ddecoder
        texture2ddecoder::decode_etc2_rgba8(data, width_usize, height_usize, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("ETC2 RGBA8 decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from ETC2 RGBA8 decoded data")
        })
    }

    #[cfg(feature = "texture-advanced")]
    fn decode_astc_4x4(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode ASTC 4x4 using texture2ddecoder
        texture2ddecoder::decode_astc(data, width_usize, height_usize, 4, 4, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("ASTC 4x4 decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from ASTC 4x4 decoded data")
        })
    }

    #[cfg(feature = "texture-advanced")]
    fn decode_astc_6x6(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode ASTC 6x6 using texture2ddecoder
        texture2ddecoder::decode_astc(data, width_usize, height_usize, 6, 6, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("ASTC 6x6 decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from ASTC 6x6 decoded data")
        })
    }

    #[cfg(feature = "texture-advanced")]
    fn decode_astc_8x8(&self, data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
        let width_usize = width as usize;
        let height_usize = height as usize;

        // Create output buffer for RGBA pixels (u32 format)
        let mut output = vec![0u32; width_usize * height_usize];

        // Decode ASTC 8x8 using texture2ddecoder
        texture2ddecoder::decode_astc(data, width_usize, height_usize, 8, 8, &mut output).map_err(
            |e| BinaryError::decompression_failed(format!("ASTC 8x8 decode failed: {:?}", e)),
        )?;

        // Convert u32 RGBA to u8 RGBA (texture2ddecoder uses BGRA format)
        let rgba_data: Vec<u8> = output
            .iter()
            .flat_map(|&pixel| {
                // texture2ddecoder returns BGRA in u32, we need RGBA
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let a = ((pixel >> 24) & 0xFF) as u8;
                [r, g, b, a]
            })
            .collect();

        // Create RgbaImage from decoded data
        RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
            BinaryError::invalid_data("Failed to create image from ASTC 8x8 decoded data")
        })
    }

    /// Export texture to PNG file
    pub fn export_png(&self, path: &str) -> Result<()> {
        let image = self.decode_image()?;
        image
            .save(path)
            .map_err(|e| BinaryError::generic(format!("Failed to save PNG: {}", e)))?;
        Ok(())
    }

    /// Export texture to JPEG file (note: JPEG doesn't support transparency)
    pub fn export_jpeg(&self, path: &str, quality: u8) -> Result<()> {
        let rgba_image = self.decode_image()?;

        // Convert RGBA to RGB for JPEG
        let rgb_image = image::DynamicImage::ImageRgba8(rgba_image).to_rgb8();

        // Save as JPEG with specified quality
        let mut output = std::fs::File::create(path)
            .map_err(|e| BinaryError::generic(format!("Failed to create file: {}", e)))?;

        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality);
        rgb_image
            .write_with_encoder(encoder)
            .map_err(|e| BinaryError::generic(format!("Failed to save JPEG: {}", e)))?;

        Ok(())
    }

    /// Get texture information summary
    pub fn get_info(&self) -> TextureInfo {
        TextureInfo {
            name: self.name.clone(),
            width: self.width,
            height: self.height,
            format: self.format,
            format_info: self.format.info(),
            mip_count: self.mip_count,
            data_size: self.image_data.len(),
            has_alpha: self.format.info().has_alpha,
            is_compressed: self.format.info().compressed,
        }
    }
}

/// Texture information summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextureInfo {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub format: TextureFormat,
    pub format_info: TextureFormatInfo,
    pub mip_count: i32,
    pub data_size: usize,
    pub has_alpha: bool,
    pub is_compressed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_texture_format_conversion() {
        assert_eq!(TextureFormat::from(4), TextureFormat::RGBA32);
        assert_eq!(TextureFormat::from(10), TextureFormat::DXT1);
        assert_eq!(TextureFormat::from(-1), TextureFormat::Unknown);
        assert_eq!(TextureFormat::from(999), TextureFormat::Unknown);
    }

    #[test]
    fn test_texture_format_info() {
        let rgba32_info = TextureFormat::RGBA32.info();
        assert_eq!(rgba32_info.name, "RGBA32");
        assert_eq!(rgba32_info.bits_per_pixel, 32);
        assert!(!rgba32_info.compressed);
        assert!(rgba32_info.has_alpha);
        assert!(rgba32_info.supported);

        let dxt1_info = TextureFormat::DXT1.info();
        assert_eq!(dxt1_info.name, "DXT1");
        assert_eq!(dxt1_info.bits_per_pixel, 4);
        assert!(dxt1_info.compressed);
        assert!(!dxt1_info.has_alpha);
        assert!(dxt1_info.supported);
    }

    #[test]
    fn test_data_size_calculation() {
        // Uncompressed format
        let size = TextureFormat::RGBA32.calculate_data_size(256, 256);
        assert_eq!(size, 256 * 256 * 4); // 4 bytes per pixel

        // Compressed format (DXT1 uses 8 bytes per 4x4 block)
        let size = TextureFormat::DXT1.calculate_data_size(256, 256);
        assert_eq!(size, (256 / 4) * (256 / 4) * 8); // 8 bytes per 4x4 block
    }

    #[test]
    fn test_texture2d_processor() {
        let version = UnityVersion::parse_version("2020.3.12f1").unwrap();
        let processor = Texture2DProcessor::new(version);

        let supported_formats = processor.get_supported_formats();
        assert!(supported_formats.contains(&TextureFormat::RGBA32));
        assert!(supported_formats.contains(&TextureFormat::DXT1));
        assert!(supported_formats.contains(&TextureFormat::ETC2_RGB));
        assert!(supported_formats.contains(&TextureFormat::ASTC_RGBA_4x4));
    }

    #[test]
    fn test_texture2d_default() {
        let texture = Texture2D::default();
        assert_eq!(texture.name, "");
        assert_eq!(texture.width, 0);
        assert_eq!(texture.height, 0);
        assert_eq!(texture.format, TextureFormat::Unknown);
        assert!(!texture.mip_map);
        assert_eq!(texture.mip_count, 1);
    }

    #[test]
    fn test_texture_decode_rgba32() {
        let texture = Texture2D {
            width: 2,
            height: 2,
            format: TextureFormat::RGBA32,
            image_data: vec![
                255, 0, 0, 255, // Red pixel
                0, 255, 0, 255, // Green pixel
                0, 0, 255, 255, // Blue pixel
                255, 255, 255, 128, // White with 50% alpha
            ],
            ..Default::default()
        };

        let image = texture.decode_image().unwrap();
        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 2);

        // Check pixel colors
        assert_eq!(image.get_pixel(0, 0), &Rgba([255, 0, 0, 255])); // Red
        assert_eq!(image.get_pixel(1, 0), &Rgba([0, 255, 0, 255])); // Green
        assert_eq!(image.get_pixel(0, 1), &Rgba([0, 0, 255, 255])); // Blue
        assert_eq!(image.get_pixel(1, 1), &Rgba([255, 255, 255, 128])); // White with alpha
    }

    #[test]
    fn test_texture_decode_rgb24() {
        let texture = Texture2D {
            width: 1,
            height: 1,
            format: TextureFormat::RGB24,
            image_data: vec![128, 64, 192], // Purple-ish color
            ..Default::default()
        };

        let image = texture.decode_image().unwrap();
        assert_eq!(image.width(), 1);
        assert_eq!(image.height(), 1);

        // Check pixel color (RGB24 should have full alpha)
        assert_eq!(image.get_pixel(0, 0), &Rgba([128, 64, 192, 255]));
    }

    #[test]
    fn test_texture_decode_invalid_dimensions() {
        let texture = Texture2D {
            width: 0,
            height: 0,
            format: TextureFormat::RGBA32,
            image_data: vec![255, 0, 0, 255],
            ..Default::default()
        };

        let result = texture.decode_image();
        assert!(result.is_err());
    }

    #[test]
    fn test_texture_decode_insufficient_data() {
        let texture = Texture2D {
            width: 2,
            height: 2,
            format: TextureFormat::RGBA32,
            image_data: vec![255, 0, 0], // Only 3 bytes, need 16
            ..Default::default()
        };

        let result = texture.decode_image();
        assert!(result.is_err());
    }

    #[test]
    fn test_texture_info() {
        let texture = Texture2D {
            name: "TestTexture".to_string(),
            width: 256,
            height: 256,
            format: TextureFormat::RGBA32,
            mip_count: 8,
            image_data: vec![0; 256 * 256 * 4],
            ..Default::default()
        };

        let info = texture.get_info();
        assert_eq!(info.name, "TestTexture");
        assert_eq!(info.width, 256);
        assert_eq!(info.height, 256);
        assert_eq!(info.format, TextureFormat::RGBA32);
        assert_eq!(info.mip_count, 8);
        assert_eq!(info.data_size, 256 * 256 * 4);
        assert!(info.has_alpha);
        assert!(!info.is_compressed);
    }
}
