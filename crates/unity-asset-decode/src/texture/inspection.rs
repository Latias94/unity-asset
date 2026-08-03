//! Strict Texture2D TypeTree inspection.

use indexmap::IndexMap;
use unity_asset_core::UnityValue;

use super::formats::TextureFormat;
use crate::media::{
    EmbeddedMediaRef, MediaInspectionError, MediaPayloadRef, StreamDataShape, stream_data_candidate,
};
use unity_asset_binary::asset::class_ids;
use unity_asset_binary::object::UnityObject;

/// Allocation-free Texture2D metadata used by preparation and planners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Texture2DLayout<'a> {
    width: u32,
    height: u32,
    format: TextureFormat,
    mip_count: u32,
    image_count: u32,
    complete_image_size: u64,
    payload: MediaPayloadRef<'a>,
}

impl<'a> Texture2DLayout<'a> {
    /// Inspects one Texture2D using only materialized TypeTree evidence.
    pub fn inspect(object: &'a UnityObject) -> Result<Self, MediaInspectionError> {
        if object.class_id() != class_ids::TEXTURE_2D {
            return Err(MediaInspectionError::NotApplicable {
                expected: class_ids::TEXTURE_2D,
                actual: object.class_id(),
            });
        }
        let properties = object.as_unity_class().properties();
        if properties.is_empty() {
            return Err(MediaInspectionError::TypeTreeUnavailable);
        }

        let width = positive_dimension(properties, "m_Width")?;
        let height = positive_dimension(properties, "m_Height")?;
        width
            .checked_mul(height)
            .ok_or(MediaInspectionError::InvalidDescriptor {
                field: "m_Width/m_Height",
                reason: "texture pixel count overflows u32",
            })?;
        let format_value = required_i32(properties, "m_TextureFormat")?;
        let format = TextureFormat::from(format_value);
        if format == TextureFormat::Unknown {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_TextureFormat",
                reason: "texture format is unknown",
            });
        }
        if format.descriptor_encoding().is_none() {
            return Err(MediaInspectionError::UnsupportedEncoding {
                family: "Texture2D",
                value: format_value,
            });
        }
        let mip_count = match properties.get("m_MipCount") {
            Some(_) => positive_u32(properties, "m_MipCount")?,
            None if matches!(properties.get("m_MipMap"), Some(UnityValue::Bool(_))) => {
                return Err(MediaInspectionError::UnsupportedLayout {
                    family: "Texture2D",
                    layout: "legacy m_MipMap mip layout",
                });
            }
            None if properties.contains_key("m_MipMap") => {
                return Err(MediaInspectionError::InvalidDescriptor {
                    field: "m_MipMap",
                    reason: "field must be a boolean",
                });
            }
            None => {
                return Err(MediaInspectionError::InvalidDescriptor {
                    field: "m_MipCount",
                    reason: "texture has no supported mip layout evidence",
                });
            }
        };
        let maximum_mip_count = u32::BITS - width.max(height).leading_zeros();
        if mip_count > maximum_mip_count {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_MipCount",
                reason: "texture mip count exceeds its dimensions",
            });
        }
        let image_count = positive_u32(properties, "m_ImageCount")?;
        if image_count != 1 {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_ImageCount",
                reason: "strict PNG preparation supports one Texture2D image",
            });
        }
        let texture_dimension = positive_u32(properties, "m_TextureDimension")?;
        if texture_dimension != 2 {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_TextureDimension",
                reason: "strict PNG preparation supports two-dimensional textures",
            });
        }
        let complete_image_size = positive_u64(properties, "m_CompleteImageSize")?;
        let expected_image_size = mip_chain_size(format, width, height, mip_count)?;
        if complete_image_size != expected_image_size {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_CompleteImageSize",
                reason: "declared texture size does not match its strict mip layout",
            });
        }

        let embedded = embedded_payload(properties)?;
        let stream = stream_data_candidate(
            properties.get("m_StreamData"),
            StreamDataShape::UNITY_STREAM_DATA,
        )?;
        let payload = MediaPayloadRef::classify(embedded, stream)?;
        let payload_size = match payload {
            MediaPayloadRef::Embedded(embedded) => {
                u64::try_from(embedded.byte_len()).map_err(|_| {
                    MediaInspectionError::InvalidDescriptor {
                        field: "image_data",
                        reason: "embedded texture length exceeds u64",
                    }
                })?
            }
            MediaPayloadRef::Streamed(stream) => stream.size(),
        };
        if payload_size != complete_image_size {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "image_data/m_StreamData",
                reason: "texture payload length does not match m_CompleteImageSize",
            });
        }

        Ok(Self {
            width,
            height,
            format,
            mip_count,
            image_count,
            complete_image_size,
            payload,
        })
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn format(self) -> TextureFormat {
        self.format
    }

    #[must_use]
    pub const fn mip_count(self) -> u32 {
        self.mip_count
    }

    #[must_use]
    pub const fn image_count(self) -> u32 {
        self.image_count
    }

    #[must_use]
    pub const fn complete_image_size(self) -> u64 {
        self.complete_image_size
    }

    #[must_use]
    pub const fn payload(self) -> MediaPayloadRef<'a> {
        self.payload
    }
}

fn embedded_payload(
    properties: &IndexMap<String, UnityValue>,
) -> Result<Option<EmbeddedMediaRef<'_>>, MediaInspectionError> {
    let mut selected = None;
    for field in ["image_data", "image data", "m_ImageData"] {
        let Some(value) = properties.get(field) else {
            continue;
        };
        if selected.is_some() {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "image_data",
                reason: "multiple embedded texture field variants are present",
            });
        }
        selected = Some(EmbeddedMediaRef::inspect(
            value,
            "image_data",
            "texture byte arrays must contain only u8 values",
            "embedded texture data must be bytes or a byte array",
        )?);
    }
    Ok(selected)
}

fn positive_dimension(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<u32, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_i64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "texture dimension must be a positive u32",
        })
}

fn positive_u32(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<u32, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be a positive u32",
        })
}

fn positive_u64(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<u64, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_u64)
        .filter(|value| *value != 0)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be a positive u64",
        })
}

fn mip_chain_size(
    format: TextureFormat,
    width: u32,
    height: u32,
    mip_count: u32,
) -> Result<u64, MediaInspectionError> {
    let mut total = 0_u64;
    let mut mip_width = width;
    let mut mip_height = height;
    for _ in 0..mip_count {
        let level = format.checked_data_size(mip_width, mip_height).ok_or(
            MediaInspectionError::InvalidDescriptor {
                field: "m_TextureFormat",
                reason: "texture format has no strict payload-size rule",
            },
        )?;
        total = total
            .checked_add(level)
            .ok_or(MediaInspectionError::InvalidDescriptor {
                field: "m_CompleteImageSize",
                reason: "texture mip-chain size overflows u64",
            })?;
        mip_width = (mip_width / 2).max(1);
        mip_height = (mip_height / 2).max(1);
    }
    Ok(total)
}

fn required_i32(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<i32, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be an i32",
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_binary::asset::ObjectInfo;
    use unity_asset_core::UnityClass;

    fn object(properties: IndexMap<String, UnityValue>) -> UnityObject {
        let class = UnityClass::with_properties(
            class_ids::TEXTURE_2D,
            "Texture2D".to_owned(),
            "1".to_owned(),
            properties,
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::TEXTURE_2D).unwrap();
        UnityObject::from_info_and_class(info, class)
    }

    fn base() -> IndexMap<String, UnityValue> {
        IndexMap::from([
            ("m_Width".to_owned(), UnityValue::Integer(2)),
            ("m_Height".to_owned(), UnityValue::Integer(2)),
            ("m_TextureFormat".to_owned(), UnityValue::Integer(4)),
            ("m_MipCount".to_owned(), UnityValue::Integer(1)),
            ("m_ImageCount".to_owned(), UnityValue::Integer(1)),
            ("m_TextureDimension".to_owned(), UnityValue::Integer(2)),
            ("m_CompleteImageSize".to_owned(), UnityValue::Integer(16)),
        ])
    }

    fn stream(path: &str, offset: u64, size: u64) -> UnityValue {
        UnityValue::Object(IndexMap::from([
            ("path".to_owned(), UnityValue::String(path.to_owned())),
            ("offset".to_owned(), UnityValue::from(offset)),
            ("size".to_owned(), UnityValue::from(size)),
        ]))
    }

    #[test]
    fn typetree_requires_exact_dimensions_format_and_payload() {
        let mut properties = base();
        properties.insert("image_data".to_owned(), UnityValue::Bytes(vec![0; 16]));
        let texture = object(properties);
        let layout = Texture2DLayout::inspect(&texture).unwrap();
        assert_eq!((layout.width(), layout.height()), (2, 2));
        assert_eq!(layout.format(), TextureFormat::RGBA32);
        assert_eq!(layout.payload().embedded_byte_len(), Some(16));
    }

    #[test]
    fn malformed_typetree_never_falls_back_or_accepts_valid_prefixes() {
        let mut properties = base();
        properties.insert(
            "image_data".to_owned(),
            UnityValue::Array(vec![UnityValue::Integer(1), UnityValue::Float(2.0)]),
        );
        assert!(matches!(
            Texture2DLayout::inspect(&object(properties)),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "image_data",
                ..
            })
        ));

        assert_eq!(
            Texture2DLayout::inspect(&object(IndexMap::new())),
            Err(MediaInspectionError::TypeTreeUnavailable)
        );
    }

    #[test]
    fn known_but_unimplemented_encoding_is_unavailable_not_malformed() {
        let mut properties = base();
        properties.insert(
            "m_TextureFormat".to_owned(),
            UnityValue::Integer(i64::from(TextureFormat::R16 as i32)),
        );
        properties.insert("m_CompleteImageSize".to_owned(), UnityValue::Integer(8));
        properties.insert("image_data".to_owned(), UnityValue::Bytes(vec![0; 8]));

        assert_eq!(
            Texture2DLayout::inspect(&object(properties)),
            Err(MediaInspectionError::UnsupportedEncoding {
                family: "Texture2D",
                value: TextureFormat::R16 as i32,
            })
        );
    }

    #[test]
    fn simultaneous_and_overflowing_payloads_are_rejected() {
        let mut dual = base();
        dual.insert("image_data".to_owned(), UnityValue::Bytes(vec![0; 16]));
        dual.insert(
            "m_StreamData".to_owned(),
            stream("archive:/CAB-a/CAB-a.resS", 0, 16),
        );
        assert_eq!(
            Texture2DLayout::inspect(&object(dual)),
            Err(MediaInspectionError::AmbiguousPayload)
        );

        let mut overflow = base();
        overflow.insert(
            "m_StreamData".to_owned(),
            stream("archive:/CAB-a/CAB-a.resS", u64::MAX, 1),
        );
        assert_eq!(
            Texture2DLayout::inspect(&object(overflow)),
            Err(MediaInspectionError::StreamRangeOverflow {
                offset: u64::MAX,
                size: 1,
            })
        );
    }

    #[test]
    fn mip_count_is_bounded_by_texture_dimensions() {
        let mut properties = base();
        properties.insert(
            "m_MipCount".to_owned(),
            UnityValue::from(u64::from(u32::MAX)),
        );
        properties.insert("m_StreamData".to_owned(), stream("a.resS", 0, 16));

        assert!(matches!(
            Texture2DLayout::inspect(&object(properties)),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "m_MipCount",
                ..
            })
        ));
    }

    #[test]
    fn legacy_mipmap_layout_is_unsupported_without_guessing_a_count() {
        let mut properties = base();
        properties.shift_remove("m_MipCount");
        properties.insert("m_MipMap".to_owned(), UnityValue::Bool(false));
        properties.insert("image_data".to_owned(), UnityValue::Bytes(vec![0; 16]));
        assert_eq!(
            Texture2DLayout::inspect(&object(properties)),
            Err(MediaInspectionError::UnsupportedLayout {
                family: "Texture2D",
                layout: "legacy m_MipMap mip layout",
            })
        );

        let mut malformed = base();
        malformed.shift_remove("m_MipCount");
        malformed.insert("m_MipMap".to_owned(), UnityValue::Float(0.0));
        assert!(matches!(
            Texture2DLayout::inspect(&object(malformed)),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "m_MipMap",
                ..
            })
        ));
    }
}
