//! Compatibility adapter from strict Texture2D TypeTree inspection to the legacy owned model.

use image::RgbaImage;
use unity_asset_binary::asset::SerializedObjectContext;
use unity_asset_binary::object::UnityObject;
use unity_asset_binary::{BinaryError, Result};
use unity_asset_core::UnityValue;

use super::decoders::TextureDecoder;
use super::inspection::Texture2DLayout;
use super::types::{StreamingInfo, Texture2D};
use crate::media::MediaPayloadRef;

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
    pub fn from_unity_object(
        &self,
        object: &UnityObject,
        context: SerializedObjectContext,
    ) -> Result<Texture2D> {
        let layout = Texture2DLayout::inspect(object, context)
            .map_err(|error| BinaryError::invalid_data(error.to_string()))?;
        self.convert_inspected_layout(object, layout)
    }

    fn convert_inspected_layout(
        &self,
        object: &UnityObject,
        layout: Texture2DLayout<'_>,
    ) -> Result<Texture2D> {
        if layout.context().requires_source_transform() {
            return Err(BinaryError::unsupported(
                "legacy Texture2D conversion cannot retain platform storage transforms; use PreparedTexturePng",
            ));
        }
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

    #[cfg(test)]
    fn convert_unity_object_for_test(
        &self,
        object: &UnityObject,
        target_platform: i32,
    ) -> Result<Texture2D> {
        let layout = Texture2DLayout::inspect_for_test(object, Some(target_platform))
            .map_err(|error| BinaryError::invalid_data(error.to_string()))?;
        self.convert_inspected_layout(object, layout)
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

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;
    use unity_asset_binary::asset::{ObjectInfo, class_ids};
    use unity_asset_core::UnityClass;

    use super::*;

    fn object(properties: IndexMap<String, UnityValue>) -> UnityObject {
        let class = UnityClass::with_properties(
            class_ids::TEXTURE_2D,
            "Texture2D".to_owned(),
            "1".to_owned(),
            properties,
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::TEXTURE_2D)
            .expect("valid standalone texture object");
        UnityObject::from_info_and_class(info, class)
    }

    #[test]
    fn converter_retains_strict_stream_reference() {
        let stream = UnityValue::Object(IndexMap::from([
            (
                "path".to_owned(),
                UnityValue::String("archive:/CAB-abc/CAB-abc.resS".to_owned()),
            ),
            ("offset".to_owned(), UnityValue::from(u64::MAX - 16)),
            ("size".to_owned(), UnityValue::Integer(16)),
        ]));
        let texture = object(IndexMap::from([
            ("m_Name".to_owned(), UnityValue::String("Tex".to_owned())),
            ("m_Width".to_owned(), UnityValue::Integer(2)),
            ("m_Height".to_owned(), UnityValue::Integer(2)),
            ("m_TextureFormat".to_owned(), UnityValue::Integer(4)),
            ("m_MipCount".to_owned(), UnityValue::Integer(1)),
            ("m_ImageCount".to_owned(), UnityValue::Integer(1)),
            ("m_TextureDimension".to_owned(), UnityValue::Integer(2)),
            ("m_CompleteImageSize".to_owned(), UnityValue::Integer(16)),
            ("m_IsReadable".to_owned(), UnityValue::Bool(true)),
            ("m_StreamData".to_owned(), stream),
        ]));

        let converted = Texture2DConverter::new()
            .convert_unity_object_for_test(&texture, 5)
            .unwrap();
        assert_eq!(converted.name, "Tex");
        assert_eq!(converted.width, 2);
        assert_eq!(converted.height, 2);
        assert!(converted.image_data.is_empty());
        assert_eq!(converted.stream_info.offset, u64::MAX - 16);
        assert_eq!(converted.stream_info.size, 16);
        assert!(converted.stream_info.path.contains("CAB-abc"));
    }

    #[test]
    fn converter_rejects_platform_transformed_texture_bytes() {
        let texture = object(IndexMap::from([
            (
                "m_Name".to_owned(),
                UnityValue::String("XboxTex".to_owned()),
            ),
            ("m_Width".to_owned(), UnityValue::Integer(1)),
            ("m_Height".to_owned(), UnityValue::Integer(1)),
            ("m_TextureFormat".to_owned(), UnityValue::Integer(7)),
            ("m_MipCount".to_owned(), UnityValue::Integer(1)),
            ("m_ImageCount".to_owned(), UnityValue::Integer(1)),
            ("m_TextureDimension".to_owned(), UnityValue::Integer(2)),
            ("m_CompleteImageSize".to_owned(), UnityValue::Integer(2)),
            ("image_data".to_owned(), UnityValue::Bytes(vec![0xf8, 0x00])),
        ]));

        let error = Texture2DConverter::new()
            .convert_unity_object_for_test(&texture, 11)
            .expect_err("legacy conversion must not silently decode Xbox byte order");
        assert!(matches!(
            error,
            BinaryError::Unsupported(message)
                if message.contains("platform storage transforms")
        ));
    }
}
