//! Texture2D converter and processor
//!
//! This module provides the main conversion logic for Unity Texture2D objects.
//! Inspired by UnityPy/export/Texture2DConverter.py

use super::decoders::TextureDecoder;
use super::formats::TextureFormat;
use super::types::Texture2D;
use crate::error::{BinaryError, Result};
use crate::media::{MediaPayloadRef, StreamDataRef, is_plausible_stream_path};
use crate::object::UnityObject;
use crate::reader::{BinaryReader, ByteOrder};
use image::RgbaImage;
use unity_asset_core::UnityValue;

/// Allocation-free Texture2D metadata used by planners and inventory tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Texture2DLayout<'a> {
    width: i32,
    height: i32,
    payload: MediaPayloadRef<'a>,
}

impl<'a> Texture2DLayout<'a> {
    /// Inspects a Texture2D without cloning its embedded or streamed media.
    pub fn inspect(obj: &'a UnityObject) -> Result<Self> {
        inspect_texture_typetree(obj).map_or_else(|| inspect_texture_binary(obj.raw_data()), Ok)
    }

    #[must_use]
    pub const fn width(self) -> i32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> i32 {
        self.height
    }

    #[must_use]
    pub const fn payload(self) -> MediaPayloadRef<'a> {
        self.payload
    }
}

/// Main texture converter
///
/// This struct handles the conversion of Unity objects to Texture2D structures
/// and provides methods for processing texture data.
pub struct Texture2DConverter {
    decoder: TextureDecoder,
}

impl Texture2DConverter {
    /// Create a new Texture2D converter
    pub fn new() -> Self {
        Self {
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
        fn as_f32(v: &UnityValue) -> Option<f32> {
            v.as_f64().map(|n| n as f32)
        }

        let props = obj.as_unity_class().properties();

        let name = props
            .get("m_Name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let width = props.get("m_Width").and_then(as_i32).unwrap_or(0);
        let height = props.get("m_Height").and_then(as_i32).unwrap_or(0);
        let complete_image_size = props
            .get("m_CompleteImageSize")
            .and_then(as_i32)
            .unwrap_or(0);
        let image_count = props.get("m_ImageCount").and_then(as_i32).unwrap_or(1);
        let texture_dimension = props
            .get("m_TextureDimension")
            .and_then(as_i32)
            .unwrap_or(2);
        let light_map_format = props.get("m_LightmapFormat").and_then(as_i32).unwrap_or(0);
        let color_space = props.get("m_ColorSpace").and_then(as_i32).unwrap_or(0);
        let is_readable = props
            .get("m_IsReadable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mip_map = props
            .get("m_MipMap")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mip_count = props.get("m_MipCount").and_then(as_i32).unwrap_or(1);
        let format = props
            .get("m_TextureFormat")
            .and_then(as_i32)
            .map(TextureFormat::from)
            .unwrap_or(TextureFormat::Unknown);

        let mut texture = Texture2D {
            name,
            width,
            height,
            complete_image_size,
            format,
            mip_map,
            mip_count,
            is_readable,
            image_count,
            texture_dimension,
            light_map_format,
            color_space,
            ..Default::default()
        };

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
        if let Some(v) = image_data_value {
            match v {
                UnityValue::Bytes(b) => {
                    texture.data_size = b.len() as i32;
                    texture.image_data = b.clone();
                }
                UnityValue::Array(items) => {
                    let mut bytes = Vec::with_capacity(items.len());
                    for item in items {
                        let Some(n) = item.as_i64() else {
                            break;
                        };
                        let Ok(b) = u8::try_from(n) else {
                            break;
                        };
                        bytes.push(b);
                    }
                    texture.data_size = bytes.len() as i32;
                    texture.image_data = bytes;
                }
                _ => {}
            }
        }

        // Streamed texture data: `m_StreamData: { path, offset, size }`
        if let Some(UnityValue::Object(stream_obj)) = props.get("m_StreamData") {
            texture.stream_info.path = stream_obj
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            texture.stream_info.offset = stream_obj
                .get("offset")
                .and_then(UnityValue::as_u64)
                .unwrap_or(0);
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
                        if is_plausible_stream_path(&path) {
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
                    if !path.is_empty() && is_plausible_stream_path(&path) && size > 0 {
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

impl Default for Texture2DConverter {
    fn default() -> Self {
        Self::new()
    }
}

fn inspect_texture_typetree(obj: &UnityObject) -> Option<Texture2DLayout<'_>> {
    let props = obj.as_unity_class().properties();
    let width = props
        .get("m_Width")
        .and_then(UnityValue::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(0);
    let height = props
        .get("m_Height")
        .and_then(UnityValue::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(0);
    (width > 0 && height > 0).then_some(())?;

    let embedded_byte_len = props
        .get("image_data")
        .or_else(|| props.get("image data"))
        .or_else(|| props.get("m_ImageData"))
        .map(typetree_texture_byte_len)
        .unwrap_or(0);
    let stream = props
        .get("m_StreamData")
        .and_then(|value| match value {
            UnityValue::Object(resource) => Some(resource),
            _ => None,
        })
        .and_then(|resource| {
            let path = resource.get("path").and_then(UnityValue::as_str)?;
            let offset = resource
                .get("offset")
                .and_then(UnityValue::as_u64)
                .unwrap_or(0);
            let size = resource
                .get("size")
                .and_then(UnityValue::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            StreamDataRef::new(path, offset, size)
        });
    let payload = MediaPayloadRef::select(embedded_byte_len, stream)?;

    Some(Texture2DLayout {
        width,
        height,
        payload,
    })
}

fn typetree_texture_byte_len(value: &UnityValue) -> usize {
    match value {
        UnityValue::Bytes(bytes) => bytes.len(),
        UnityValue::Array(items) => items
            .iter()
            .map_while(|item| item.as_i64().and_then(|value| u8::try_from(value).ok()))
            .count(),
        _ => 0,
    }
}

fn inspect_texture_binary(data: &[u8]) -> Result<Texture2DLayout<'_>> {
    if data.is_empty() {
        return Err(BinaryError::invalid_data("Empty texture data"));
    }

    let mut reader = BinaryReader::new(data, ByteOrder::Little);
    reader.read_aligned_string_ref()?;
    let width = reader.read_i32()?;
    let height = reader.read_i32()?;
    reader.read_i32()?;
    reader.read_i32()?;
    reader.read_bool()?;
    reader.read_bool()?;
    reader.align()?;
    validate_dimensions(width, height)?;

    let declared_size = reader.read_i32()?;
    let embedded_byte_len = usize::try_from(declared_size)
        .ok()
        .filter(|size| *size != 0 && reader.remaining() >= *size);
    let payload = if let Some(byte_len) = embedded_byte_len {
        reader.skip_bytes(byte_len)?;
        MediaPayloadRef::Embedded { byte_len }
    } else if let Some(stream) = try_read_texture_stream(&mut reader) {
        MediaPayloadRef::Streamed(stream)
    } else if reader.remaining() != 0 {
        MediaPayloadRef::Embedded {
            byte_len: reader.remaining(),
        }
    } else {
        return Err(BinaryError::invalid_data(
            "Texture2D did not contain embedded bytes or stream data",
        ));
    };

    Ok(Texture2DLayout {
        width,
        height,
        payload,
    })
}

fn try_read_texture_stream<'a>(reader: &mut BinaryReader<'a>) -> Option<StreamDataRef<'a>> {
    let position = reader.position();
    if let Some(stream) = try_read_path_first_texture_stream(reader) {
        return Some(stream);
    }
    reader.set_position(position).ok()?;

    let stream = (|| {
        let offset = reader.read_u64().ok()?;
        let size = reader.read_u32().ok()?;
        let path = reader.read_aligned_string_ref().ok()?;
        StreamDataRef::new(path, offset, size)
            .filter(|stream| is_plausible_stream_path(stream.path()))
    })();
    if stream.is_some() {
        return stream;
    }
    reader.set_position(position).ok()?;
    None
}

fn try_read_path_first_texture_stream<'a>(
    reader: &mut BinaryReader<'a>,
) -> Option<StreamDataRef<'a>> {
    let path = reader.read_aligned_string_ref().ok()?;
    if !is_plausible_stream_path(path) {
        return None;
    }
    let offset = reader.read_u64().ok()?;
    let size = reader.read_u32().ok()?;
    let _ = reader.align();
    StreamDataRef::new(path, offset, size)
}

fn validate_dimensions(width: i32, height: i32) -> Result<()> {
    if width <= 0 || height <= 0 {
        return Err(BinaryError::invalid_data(
            "Texture2D missing positive dimensions",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::asset::{ObjectInfo, class_ids};
    use indexmap::IndexMap;
    use unity_asset_core::UnityClass;

    #[test]
    fn typetree_layout_counts_embedded_texture_without_owning_it() {
        let class = UnityClass::with_properties(
            class_ids::TEXTURE_2D,
            "Texture2D".to_string(),
            "1".to_string(),
            IndexMap::from([
                ("m_Width".to_string(), UnityValue::Integer(512)),
                ("m_Height".to_string(), UnityValue::Integer(512)),
                (
                    "image_data".to_string(),
                    UnityValue::Bytes(vec![3; 1024 * 1024]),
                ),
            ]),
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::TEXTURE_2D)
            .expect("valid standalone texture object");
        let object = UnityObject::from_info_and_class(info, class);

        let layout = Texture2DLayout::inspect(&object).unwrap();

        assert_eq!(layout.width(), 512);
        assert_eq!(layout.height(), 512);
        assert_eq!(
            layout.payload(),
            MediaPayloadRef::Embedded {
                byte_len: 1024 * 1024
            }
        );
    }

    #[test]
    fn raw_layout_borrows_stream_path() {
        let path = "archive:/CAB-a/CAB-a.resS";
        let mut data = Vec::new();
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&4_i32.to_le_bytes());
        data.extend_from_slice(&8_i32.to_le_bytes());
        data.extend_from_slice(&128_i32.to_le_bytes());
        data.extend_from_slice(&4_i32.to_le_bytes());
        data.extend_from_slice(&[0, 1, 0, 0]);
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&(path.len() as i32).to_le_bytes());
        let path_offset = data.len();
        data.extend_from_slice(path.as_bytes());
        while data.len() % 4 != 0 {
            data.push(0);
        }
        data.extend_from_slice(&17_u64.to_le_bytes());
        data.extend_from_slice(&23_u32.to_le_bytes());

        let layout = inspect_texture_binary(&data).unwrap();
        let stream = layout.payload().stream().unwrap();

        assert_eq!(stream.path(), path);
        assert_eq!(stream.path().as_ptr(), data[path_offset..].as_ptr());
        assert_eq!(stream.offset(), 17);
        assert_eq!(stream.size(), 23);
    }
}
