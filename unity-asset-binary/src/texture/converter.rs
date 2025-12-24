//! Texture2D converter and processor
//!
//! This module provides the main conversion logic for Unity Texture2D objects.
//! Inspired by UnityPy/export/Texture2DConverter.py

use super::decoders::TextureDecoder;
use super::formats::TextureFormat;
use super::types::Texture2D;
use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::unity_version::UnityVersion;
use image::RgbaImage;
use unity_asset_core::UnityValue;

/// Main texture converter
///
/// This struct handles the conversion of Unity objects to Texture2D structures
/// and provides methods for processing texture data.
pub struct Texture2DConverter {
    #[allow(dead_code)]
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
        // Prefer TypeTree when available; this is much more reliable for streamed textures.
        if let Ok(texture) = self.try_parse_typetree(obj) {
            return Ok(texture);
        }

        // Fallback: raw binary parsing (best-effort; version-dependent).
        self.parse_binary_data(obj.raw_data())
    }

    fn try_parse_typetree(&self, obj: &UnityObject) -> Result<Texture2D> {
        fn as_i32(v: &UnityValue) -> Option<i32> {
            v.as_i64().and_then(|n| i32::try_from(n).ok())
        }
        fn as_u32(v: &UnityValue) -> Option<u32> {
            v.as_i64().and_then(|n| u32::try_from(n).ok())
        }
        fn as_u64(v: &UnityValue) -> Option<u64> {
            v.as_i64().and_then(|n| u64::try_from(n).ok())
        }
        fn as_f32(v: &UnityValue) -> Option<f32> {
            v.as_f64().map(|n| n as f32)
        }

        let props = obj.class.properties();

        let mut texture = Texture2D::default();

        texture.name = props
            .get("m_Name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        texture.width = props.get("m_Width").and_then(as_i32).unwrap_or(0);
        texture.height = props.get("m_Height").and_then(as_i32).unwrap_or(0);
        texture.complete_image_size = props
            .get("m_CompleteImageSize")
            .and_then(as_i32)
            .unwrap_or(0);
        texture.image_count = props.get("m_ImageCount").and_then(as_i32).unwrap_or(1);
        texture.texture_dimension = props
            .get("m_TextureDimension")
            .and_then(as_i32)
            .unwrap_or(2);
        texture.light_map_format = props.get("m_LightmapFormat").and_then(as_i32).unwrap_or(0);
        texture.color_space = props.get("m_ColorSpace").and_then(as_i32).unwrap_or(0);
        texture.is_readable = props
            .get("m_IsReadable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        texture.mip_map = props
            .get("m_MipMap")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        texture.mip_count = props.get("m_MipCount").and_then(as_i32).unwrap_or(1);

        if let Some(fmt) = props.get("m_TextureFormat").and_then(as_i32) {
            texture.format = TextureFormat::from(fmt);
        }

        if let Some(UnityValue::Object(settings)) = props.get("m_TextureSettings") {
            texture.texture_settings.filter_mode =
                settings.get("m_FilterMode").and_then(as_i32).unwrap_or(0);
            texture.texture_settings.aniso = settings.get("m_Aniso").and_then(as_i32).unwrap_or(0);
            texture.texture_settings.mip_bias =
                settings.get("m_MipBias").and_then(as_f32).unwrap_or(0.0);
            texture.texture_settings.wrap_u = settings.get("m_WrapU").and_then(as_i32).unwrap_or(0);
            texture.texture_settings.wrap_v = settings.get("m_WrapV").and_then(as_i32).unwrap_or(0);
            texture.texture_settings.wrap_w = settings.get("m_WrapW").and_then(as_i32).unwrap_or(0);
        }

        // Embedded bytes (`image_data` in UnityPy; some TypeTrees may use "image data").
        let image_data_value = props
            .get("image_data")
            .or_else(|| props.get("image data"))
            .or_else(|| props.get("m_ImageData"));
        if let Some(UnityValue::Array(items)) = image_data_value {
            let mut bytes = Vec::with_capacity(items.len());
            for item in items {
                if let Some(n) = item.as_i64()
                    && let Ok(b) = u8::try_from(n)
                {
                    bytes.push(b);
                } else {
                    break;
                }
            }
            texture.data_size = bytes.len() as i32;
            texture.image_data = bytes;
        }

        // Streamed texture data: `m_StreamData: { path, offset, size }`
        if let Some(UnityValue::Object(stream_obj)) = props.get("m_StreamData") {
            texture.stream_info.path = stream_obj
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            texture.stream_info.offset = stream_obj.get("offset").and_then(as_u64).unwrap_or(0);
            texture.stream_info.size = stream_obj.get("size").and_then(as_u32).unwrap_or(0);
        }

        if texture.width <= 0 || texture.height <= 0 {
            return Err(BinaryError::invalid_data(
                "Texture2D typetree missing dimensions",
            ));
        }

        if texture.image_data.is_empty() && !texture.is_streamed() {
            return Err(BinaryError::invalid_data(
                "Texture2D typetree did not contain image bytes or stream data",
            ));
        }

        Ok(texture)
    }

    /// Parse Texture2D from raw binary data (simplified version)
    fn parse_binary_data(&self, data: &[u8]) -> Result<Texture2D> {
        if data.is_empty() {
            return Err(BinaryError::invalid_data("Empty texture data"));
        }

        let mut reader = crate::reader::BinaryReader::new(data, crate::reader::ByteOrder::Little);

        // Complex initialization with potential failures - allow field reassignment
        #[allow(clippy::field_reassign_with_default)]
        {
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
            let _ = reader.align();

            // Read data size and image data
            texture.data_size = reader.read_i32().unwrap_or(0);
            if texture.data_size > 0 && reader.remaining() >= texture.data_size as usize {
                texture.image_data = reader
                    .read_bytes(texture.data_size as usize)
                    .unwrap_or_default();
                let _ = reader.align();
            }

            // If there is no embedded image data, try to parse `m_StreamData` (best-effort).
            if texture.image_data.is_empty() && reader.remaining() >= 8 + 4 {
                let try_parse_streamdata = |reader: &mut crate::reader::BinaryReader<'_>| {
                    let pos = reader.position();

                    // Attempt 1: `path (aligned string) -> offset (u64) -> size (u32)`
                    if let Ok(path) = reader.read_aligned_string() {
                        let looks_like_path = path.is_empty()
                            || path.contains("archive:/")
                            || path.contains('/')
                            || path.contains('\\')
                            || path.ends_with(".resS")
                            || path.ends_with(".resource");
                        if looks_like_path {
                            let offset = reader.read_u64().unwrap_or(0);
                            let size = reader.read_u32().unwrap_or(0);
                            let _ = reader.align();
                            if !path.is_empty() && size > 0 {
                                return Some((path, offset, size));
                            }
                        }
                    }

                    let _ = reader.set_position(pos);

                    // Attempt 2: `offset (u64) -> size (u32) -> path (aligned string)`
                    let offset = reader.read_u64().ok()?;
                    let size = reader.read_u32().ok()?;
                    let path = reader.read_aligned_string().ok()?;
                    let looks_like_path = path.is_empty()
                        || path.contains("archive:/")
                        || path.contains('/')
                        || path.contains('\\')
                        || path.ends_with(".resS")
                        || path.ends_with(".resource");
                    if !path.is_empty() && looks_like_path && size > 0 {
                        return Some((path, offset, size));
                    }

                    None
                };

                if let Some((path, offset, size)) = try_parse_streamdata(&mut reader) {
                    texture.stream_info.path = path;
                    texture.stream_info.offset = offset;
                    texture.stream_info.size = size;
                } else if reader.remaining() > 0 {
                    // Fallback: take all remaining data as image bytes (only when not streamed).
                    let remaining_data = reader.read_remaining();
                    texture.image_data = remaining_data.to_vec();
                    texture.data_size = texture.image_data.len() as i32;
                }
            } else if texture.image_data.is_empty() && reader.remaining() > 0 {
                // Fallback: take all remaining data.
                let remaining_data = reader.read_remaining();
                texture.image_data = remaining_data.to_vec();
                texture.data_size = texture.image_data.len() as i32;
            }

            Ok(texture)
        }
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
