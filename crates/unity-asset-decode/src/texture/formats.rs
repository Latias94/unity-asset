//! Texture format definitions
//!
//! This module defines Unity texture formats and their capabilities.
//! Inspired by UnityPy/enums/TextureFormat.py

use serde::{Deserialize, Serialize};

use crate::descriptor::UnityTextureEncoding;

/// Unity texture formats
///
/// This enum represents all texture formats supported by Unity.
/// Values match Unity's internal TextureFormat enum.
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
            64 => TextureFormat::ETC_RGB4Crunched,
            65 => TextureFormat::ETC2_RGBA8Crunched,
            _ => TextureFormat::Unknown,
        }
    }
}

/// Texture format capabilities and metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextureFormatInfo {
    pub name: String,
    pub bits_per_pixel: u32,
    pub block_size: (u32, u32), // (width, height) in pixels
    pub compressed: bool,
    pub has_alpha: bool,
    pub supported: bool,
}

impl Default for TextureFormatInfo {
    fn default() -> Self {
        Self {
            name: "Unknown".to_string(),
            bits_per_pixel: 0,
            block_size: (1, 1),
            compressed: false,
            has_alpha: false,
            supported: false,
        }
    }
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
            TextureFormat::BGRA32 => TextureFormatInfo {
                name: "BGRA32".to_string(),
                bits_per_pixel: 32,
                block_size: (1, 1),
                compressed: false,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::RGBA4444 => TextureFormatInfo {
                name: "RGBA4444".to_string(),
                bits_per_pixel: 16,
                block_size: (1, 1),
                compressed: false,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::ARGB4444 => TextureFormatInfo {
                name: "ARGB4444".to_string(),
                bits_per_pixel: 16,
                block_size: (1, 1),
                compressed: false,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::RGB565 => TextureFormatInfo {
                name: "RGB565".to_string(),
                bits_per_pixel: 16,
                block_size: (1, 1),
                compressed: false,
                has_alpha: false,
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
            TextureFormat::BC4 => TextureFormatInfo {
                name: "BC4".to_string(),
                bits_per_pixel: 4,
                block_size: (4, 4),
                compressed: true,
                has_alpha: false,
                supported: cfg!(feature = "texture-advanced"),
            },
            TextureFormat::BC5 => TextureFormatInfo {
                name: "BC5".to_string(),
                bits_per_pixel: 8,
                block_size: (4, 4),
                compressed: true,
                has_alpha: false,
                supported: cfg!(feature = "texture-advanced"),
            },
            TextureFormat::BC7 => TextureFormatInfo {
                name: "BC7".to_string(),
                bits_per_pixel: 8,
                block_size: (4, 4),
                compressed: true,
                has_alpha: true,
                supported: cfg!(feature = "texture-advanced"),
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
            TextureFormat::ASTC_RGBA_6x6 => TextureFormatInfo {
                name: "ASTC_RGBA_6x6".to_string(),
                bits_per_pixel: 4,
                block_size: (6, 6),
                compressed: true,
                has_alpha: true,
                supported: true,
            },
            TextureFormat::ASTC_RGBA_8x8 => TextureFormatInfo {
                name: "ASTC_RGBA_8x8".to_string(),
                bits_per_pixel: 2,
                block_size: (8, 8),
                compressed: true,
                has_alpha: true,
                supported: true,
            },
            _ => TextureFormatInfo::default(),
        }
    }

    /// Check if format is supported for decoding
    pub fn is_supported(&self) -> bool {
        self.info().supported
    }

    /// Get expected base-level data size for given dimensions.
    pub fn calculate_data_size(&self, width: u32, height: u32) -> u32 {
        self.checked_data_size(width, height)
            .and_then(|bytes| u32::try_from(bytes).ok())
            .unwrap_or(0)
    }

    /// Returns the exact base-level payload size for formats with a strict decoder.
    pub fn checked_data_size(&self, width: u32, height: u32) -> Option<u64> {
        let (block_width, block_height, bytes_per_block) = match self {
            Self::Alpha8 => (1, 1, 1),
            Self::RGB24 => (1, 1, 3),
            Self::RGBA32 | Self::ARGB32 | Self::BGRA32 => (1, 1, 4),
            Self::RGBA4444 | Self::ARGB4444 | Self::RGB565 => (1, 1, 2),
            Self::DXT1 | Self::BC4 | Self::ETC2_RGB => (4, 4, 8),
            Self::DXT5 | Self::BC5 | Self::BC7 | Self::ETC2_RGBA8 => (4, 4, 16),
            Self::ASTC_RGBA_4x4 => (4, 4, 16),
            Self::ASTC_RGBA_6x6 => (6, 6, 16),
            Self::ASTC_RGBA_8x8 => (8, 8, 16),
            _ => return None,
        };
        let blocks_x = u64::from(width.div_ceil(block_width));
        let blocks_y = u64::from(height.div_ceil(block_height));
        blocks_x.checked_mul(blocks_y)?.checked_mul(bytes_per_block)
    }

    /// Maps formats with a strict decoder into the stable media descriptor domain.
    #[must_use]
    pub const fn descriptor_encoding(self) -> Option<UnityTextureEncoding> {
        match self {
            Self::Alpha8 => Some(UnityTextureEncoding::Alpha8),
            Self::ARGB4444 => Some(UnityTextureEncoding::Argb4444),
            Self::RGB24 => Some(UnityTextureEncoding::Rgb24),
            Self::RGBA32 => Some(UnityTextureEncoding::Rgba32),
            Self::ARGB32 => Some(UnityTextureEncoding::Argb32),
            Self::RGB565 => Some(UnityTextureEncoding::Rgb565),
            #[cfg(feature = "texture-advanced")]
            Self::DXT1 => Some(UnityTextureEncoding::Dxt1),
            #[cfg(feature = "texture-advanced")]
            Self::DXT5 => Some(UnityTextureEncoding::Dxt5),
            Self::RGBA4444 => Some(UnityTextureEncoding::Rgba4444),
            Self::BGRA32 => Some(UnityTextureEncoding::Bgra32),
            #[cfg(feature = "texture-advanced")]
            Self::BC4 => Some(UnityTextureEncoding::Bc4),
            #[cfg(feature = "texture-advanced")]
            Self::BC5 => Some(UnityTextureEncoding::Bc5),
            #[cfg(feature = "texture-advanced")]
            Self::BC7 => Some(UnityTextureEncoding::Bc7),
            #[cfg(feature = "texture-advanced")]
            Self::ETC2_RGB => Some(UnityTextureEncoding::Etc2Rgb),
            #[cfg(feature = "texture-advanced")]
            Self::ETC2_RGBA8 => Some(UnityTextureEncoding::Etc2Rgba8),
            #[cfg(feature = "texture-advanced")]
            Self::ASTC_RGBA_4x4 => Some(UnityTextureEncoding::AstcRgba4x4),
            #[cfg(feature = "texture-advanced")]
            Self::ASTC_RGBA_6x6 => Some(UnityTextureEncoding::AstcRgba6x6),
            #[cfg(feature = "texture-advanced")]
            Self::ASTC_RGBA_8x8 => Some(UnityTextureEncoding::AstcRgba8x8),
            _ => None,
        }
    }

    /// Check if format uses Crunch compression
    pub fn is_crunch_compressed(&self) -> bool {
        matches!(
            self,
            TextureFormat::DXT1Crunched
                | TextureFormat::DXT5Crunched
                | TextureFormat::ETC_RGB4Crunched
                | TextureFormat::ETC2_RGBA8Crunched
        )
    }

    /// Check if format is a basic uncompressed format
    pub fn is_basic_format(&self) -> bool {
        matches!(
            self,
            TextureFormat::Alpha8
                | TextureFormat::RGB24
                | TextureFormat::RGBA32
                | TextureFormat::ARGB32
                | TextureFormat::BGRA32
                | TextureFormat::RGBA4444
                | TextureFormat::ARGB4444
                | TextureFormat::RGB565
                | TextureFormat::R16
        )
    }

    /// Check if format is a compressed format
    pub fn is_compressed_format(&self) -> bool {
        matches!(
            self,
            TextureFormat::DXT1
                | TextureFormat::DXT5
                | TextureFormat::BC4
                | TextureFormat::BC5
                | TextureFormat::BC6H
                | TextureFormat::BC7
        )
    }

    /// Check if format is a mobile-specific format
    pub fn is_mobile_format(&self) -> bool {
        matches!(
            self,
            TextureFormat::PVRTC_RGB2
                | TextureFormat::PVRTC_RGBA2
                | TextureFormat::PVRTC_RGB4
                | TextureFormat::PVRTC_RGBA4
                | TextureFormat::ETC_RGB4
                | TextureFormat::ETC2_RGB
                | TextureFormat::ETC2_RGBA1
                | TextureFormat::ETC2_RGBA8
                | TextureFormat::EAC_R
                | TextureFormat::EAC_R_SIGNED
                | TextureFormat::EAC_RG
                | TextureFormat::EAC_RG_SIGNED
                | TextureFormat::ASTC_RGB_4x4
                | TextureFormat::ASTC_RGB_5x5
                | TextureFormat::ASTC_RGB_6x6
                | TextureFormat::ASTC_RGB_8x8
                | TextureFormat::ASTC_RGB_10x10
                | TextureFormat::ASTC_RGB_12x12
                | TextureFormat::ASTC_RGBA_4x4
                | TextureFormat::ASTC_RGBA_5x5
                | TextureFormat::ASTC_RGBA_6x6
                | TextureFormat::ASTC_RGBA_8x8
                | TextureFormat::ASTC_RGBA_10x10
                | TextureFormat::ASTC_RGBA_12x12
        )
    }
}
