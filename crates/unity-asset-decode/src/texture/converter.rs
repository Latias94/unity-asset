//! Compatibility adapter from strict Texture2D TypeTree inspection to the legacy owned model.

use image::RgbaImage;
use unity_asset_core::UnityValue;

use super::decoders::TextureDecoder;
use super::inspection::Texture2DLayout;
use super::types::{StreamingInfo, Texture2D};
use crate::media::MediaPayloadRef;
use unity_asset_binary::object::UnityObject;
use unity_asset_binary::{BinaryError, Result};

/// Legacy owned Texture2D adapter.
///
/// New code should use [`Texture2DLayout`] and prepared media writers directly.
pub struct Texture2DConverter {
    decoder: TextureDecoder,
}

impl Texture2DConverter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            decoder: TextureDecoder::new(),
        }
    }

    /// Converts a strictly inspected TypeTree-backed object into the legacy owned model.
    pub fn from_unity_object(&self, object: &UnityObject) -> Result<Texture2D> {
        let layout = Texture2DLayout::inspect(object)
            .map_err(|error| BinaryError::invalid_data(error.to_string()))?;
        let properties = object.as_unity_class().properties();
        let mut texture = Texture2D {
            name: properties
                .get("m_Name")
                .and_then(UnityValue::as_str)
                .unwrap_or_default()
                .to_owned(),
            width: i32::try_from(layout.width())
                .map_err(|_| BinaryError::invalid_data("texture width exceeds i32"))?,
            height: i32::try_from(layout.height())
                .map_err(|_| BinaryError::invalid_data("texture height exceeds i32"))?,
            complete_image_size: optional_i32(properties.get("m_CompleteImageSize"), 0),
            format: layout.format(),
            mip_map: optional_bool(properties.get("m_MipMap"), false),
            mip_count: optional_i32(properties.get("m_MipCount"), 1),
            is_readable: optional_bool(properties.get("m_IsReadable"), false),
            image_count: optional_i32(properties.get("m_ImageCount"), 1),
            texture_dimension: optional_i32(properties.get("m_TextureDimension"), 2),
            light_map_format: optional_i32(properties.get("m_LightmapFormat"), 0),
            color_space: optional_i32(properties.get("m_ColorSpace"), 0),
            ..Texture2D::default()
        };

        match layout.payload() {
            MediaPayloadRef::Embedded(_) => {
                let value = ["image_data", "image data", "m_ImageData"]
                    .into_iter()
                    .find_map(|field| properties.get(field))
                    .expect("strict Texture2D inspection validates embedded payload presence");
                texture.image_data = owned_bytes(value)?;
                texture.data_size = i32::try_from(texture.image_data.len()).map_err(|_| {
                    BinaryError::invalid_data("legacy Texture2D data length exceeds i32")
                })?;
            }
            MediaPayloadRef::Streamed(stream) => {
                let size = u32::try_from(stream.size()).map_err(|_| {
                    BinaryError::invalid_data("legacy Texture2D stream size exceeds u32")
                })?;
                texture.stream_info = StreamingInfo {
                    offset: stream.offset(),
                    size,
                    path: stream.path().to_owned(),
                };
            }
        }
        Ok(texture)
    }

    pub fn decode_to_image(&self, texture: &Texture2D) -> Result<RgbaImage> {
        self.decoder.decode(texture)
    }
}

impl Default for Texture2DConverter {
    fn default() -> Self {
        Self::new()
    }
}

fn owned_bytes(value: &UnityValue) -> Result<Vec<u8>> {
    match value {
        UnityValue::Bytes(bytes) => Ok(bytes.clone()),
        UnityValue::Array(items) => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| {
                        BinaryError::invalid_data("Texture2D byte array contains a non-u8 value")
                    })
            })
            .collect(),
        _ => Err(BinaryError::invalid_data(
            "Texture2D embedded payload is not bytes",
        )),
    }
}

fn optional_i32(value: Option<&UnityValue>, default: i32) -> i32 {
    value
        .and_then(UnityValue::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(default)
}

fn optional_bool(value: Option<&UnityValue>, default: bool) -> bool {
    value.and_then(UnityValue::as_bool).unwrap_or(default)
}
