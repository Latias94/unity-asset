//! Strict Texture2D TypeTree inspection.

use indexmap::IndexMap;
use unity_asset_core::UnityValue;

use super::formats::TextureFormat;
use crate::media::{
    EmbeddedMediaRef, MediaInspectionError, MediaPayloadRef, StreamDataShape, stream_data_candidate,
};
use unity_asset_binary::asset::{
    BuildTarget, SerializedObjectContext, TargetPlatformEvidence, class_ids,
};
use unity_asset_binary::object::UnityObject;

const BUILD_TARGET_XBOX_360: i32 = BuildTarget::XBOX_360.raw();
const BUILD_TARGET_SWITCH: i32 = BuildTarget::SWITCH.raw();
const BUILD_TARGET_SWITCH_2: i32 = BuildTarget::SWITCH_2.raw();
const MAX_SWITCH_BLOCK_HEIGHT_LOG2: u32 = 5;

/// Resolved platform storage evidence retained by a strict Texture2D layout.
///
/// This value has no public constructor. It can only be produced by inspecting an object with a
/// file-owned [`SerializedObjectContext`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaInspectionContext {
    target_platform: i32,
    storage: TextureStorageLayout,
}

impl MediaInspectionContext {
    /// Returns the exact target-platform value proven by the owning SerializedFile.
    #[must_use]
    pub const fn target_platform(self) -> i32 {
        self.target_platform
    }

    /// Returns whether the encoded bytes require a platform transform before decoding.
    #[must_use]
    pub const fn requires_source_transform(self) -> bool {
        matches!(
            self.storage,
            TextureStorageLayout::Xbox360WordSwapped
                | TextureStorageLayout::SwitchBlockLinear { .. }
        )
    }

    fn resolve_texture_storage(
        target_platform: Option<i32>,
        properties: &IndexMap<String, UnityValue>,
        format: TextureFormat,
        width: u32,
        height: u32,
        mip_count: u32,
    ) -> Result<Self, MediaInspectionError> {
        let Some(target_platform) = target_platform else {
            return Err(MediaInspectionError::UnsupportedLayout {
                family: "Texture2D",
                layout: "SerializedFile target-platform metadata is absent",
            });
        };
        let storage = match target_platform {
            BUILD_TARGET_XBOX_360 if xbox_word_swapped(format) => {
                TextureStorageLayout::Xbox360WordSwapped
            }
            BUILD_TARGET_XBOX_360 => TextureStorageLayout::Linear,
            BUILD_TARGET_SWITCH => switch_storage(properties, format, width, height, mip_count)?,
            BUILD_TARGET_SWITCH_2 => {
                return Err(MediaInspectionError::UnsupportedLayout {
                    family: "Texture2D",
                    layout: "Nintendo Switch 2 texture storage",
                });
            }
            target if is_proven_linear_target(target) => TextureStorageLayout::Linear,
            _ => {
                return Err(MediaInspectionError::UnsupportedLayout {
                    family: "Texture2D",
                    layout: "unproven target-platform texture storage",
                });
            }
        };
        Ok(Self {
            target_platform,
            storage,
        })
    }

    pub(crate) const fn storage(self) -> TextureStorageLayout {
        self.storage
    }

    const fn platform_copy_bytes(self, source_length: u64) -> u64 {
        match self.storage {
            TextureStorageLayout::SwitchBlockLinear { .. } => source_length,
            TextureStorageLayout::Linear
            | TextureStorageLayout::Xbox360WordSwapped
            | TextureStorageLayout::SwitchLinear => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextureStorageLayout {
    Linear,
    Xbox360WordSwapped,
    SwitchLinear,
    SwitchBlockLinear {
        block_width: u8,
        block_height: u8,
        gobs_per_block: u8,
    },
}

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
    context: MediaInspectionContext,
}

impl<'a> Texture2DLayout<'a> {
    /// Inspects one Texture2D using materialized TypeTree and owning SerializedFile evidence.
    pub fn inspect(
        object: &'a UnityObject,
        context: SerializedObjectContext,
    ) -> Result<Self, MediaInspectionError> {
        let target_platform = match context.target_platform() {
            TargetPlatformEvidence::Absent => None,
            TargetPlatformEvidence::Present(target) => Some(target.raw()),
        };
        Self::inspect_with_target_platform(object, target_platform)
    }

    fn inspect_with_target_platform(
        object: &'a UnityObject,
        target_platform: Option<i32>,
    ) -> Result<Self, MediaInspectionError> {
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
        let context = MediaInspectionContext::resolve_texture_storage(
            target_platform,
            properties,
            format,
            width,
            height,
            mip_count,
        )?;
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
            context,
        })
    }

    #[cfg(test)]
    pub(crate) fn inspect_for_test(
        object: &'a UnityObject,
        target_platform: Option<i32>,
    ) -> Result<Self, MediaInspectionError> {
        Self::inspect_with_target_platform(object, target_platform)
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

    /// Returns the additional retained bytes required to normalize platform storage.
    #[must_use]
    pub const fn platform_copy_bytes(self) -> u64 {
        self.context.platform_copy_bytes(self.complete_image_size)
    }

    #[must_use]
    pub const fn payload(self) -> MediaPayloadRef<'a> {
        self.payload
    }

    #[must_use]
    pub const fn context(self) -> MediaInspectionContext {
        self.context
    }
}

const fn is_proven_linear_target(target: i32) -> bool {
    matches!(
        target,
        -2 | 2
            | 3
            | 4
            | 5
            | 6
            | 7
            | 9
            | 13
            | 14
            | 15
            | 16
            | 17
            | 18
            | 19
            | 20
            | 21
            | 22
            | 23
            | 24
            | 25
            | 26
            | 27
            | 28
            | 29
            | 37
            | 39
            | 40
            | 41
            | 45
            | 46
            | 47
    )
}

const fn xbox_word_swapped(format: TextureFormat) -> bool {
    matches!(
        format,
        TextureFormat::ARGB4444
            | TextureFormat::RGB565
            | TextureFormat::DXT1
            | TextureFormat::DXT5
            | TextureFormat::DXT1Crunched
            | TextureFormat::DXT5Crunched
    )
}

fn switch_storage(
    properties: &IndexMap<String, UnityValue>,
    format: TextureFormat,
    width: u32,
    height: u32,
    mip_count: u32,
) -> Result<TextureStorageLayout, MediaInspectionError> {
    let block_height_log2 = switch_block_height_log2(properties.get("m_PlatformBlob"))?;
    if block_height_log2 == 0 {
        return Ok(TextureStorageLayout::SwitchLinear);
    }
    if mip_count != 1 {
        return Err(MediaInspectionError::UnsupportedLayout {
            family: "Texture2D",
            layout: "Nintendo Switch block-linear mip chains",
        });
    }
    let Some((block_width, block_height)) = switch_storage_block(format) else {
        return Err(MediaInspectionError::UnsupportedLayout {
            family: "Texture2D",
            layout: "Nintendo Switch block-linear encoding",
        });
    };
    let gobs_per_block =
        1_u32
            .checked_shl(block_height_log2)
            .ok_or(MediaInspectionError::UnsupportedLayout {
                family: "Texture2D",
                layout: "Nintendo Switch block height",
            })?;
    let tile_width = block_width
        .checked_mul(4)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field: "m_PlatformBlob",
            reason: "Nintendo Switch tile width overflows u32",
        })?;
    let tile_height = block_height
        .checked_mul(8)
        .and_then(|height| height.checked_mul(gobs_per_block))
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field: "m_PlatformBlob",
            reason: "Nintendo Switch tile height overflows u32",
        })?;
    if !width.is_multiple_of(tile_width) || !height.is_multiple_of(tile_height) {
        return Err(MediaInspectionError::UnsupportedLayout {
            family: "Texture2D",
            layout: "Nintendo Switch block-linear edge padding",
        });
    }
    let block_width =
        u8::try_from(block_width).map_err(|_| MediaInspectionError::InvalidDescriptor {
            field: "m_TextureFormat",
            reason: "Nintendo Switch storage block width exceeds u8",
        })?;
    let block_height =
        u8::try_from(block_height).map_err(|_| MediaInspectionError::InvalidDescriptor {
            field: "m_TextureFormat",
            reason: "Nintendo Switch storage block height exceeds u8",
        })?;
    let gobs_per_block =
        u8::try_from(gobs_per_block).map_err(|_| MediaInspectionError::InvalidDescriptor {
            field: "m_PlatformBlob",
            reason: "Nintendo Switch block height exceeds u8",
        })?;
    Ok(TextureStorageLayout::SwitchBlockLinear {
        block_width,
        block_height,
        gobs_per_block,
    })
}

fn switch_block_height_log2(value: Option<&UnityValue>) -> Result<u32, MediaInspectionError> {
    let Some(value) = value else {
        return Err(MediaInspectionError::UnsupportedLayout {
            family: "Texture2D",
            layout: "Nintendo Switch texture without m_PlatformBlob evidence",
        });
    };
    let mut exponent = [0_u8; 4];
    match value {
        UnityValue::Bytes(bytes) => {
            let Some(encoded) = bytes.get(8..12) else {
                return Err(MediaInspectionError::UnsupportedLayout {
                    family: "Texture2D",
                    layout: "Nintendo Switch texture with a short m_PlatformBlob",
                });
            };
            exponent.copy_from_slice(encoded);
        }
        UnityValue::Array(values) => {
            if values.iter().any(|value| {
                value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .is_none()
            }) {
                return Err(MediaInspectionError::InvalidDescriptor {
                    field: "m_PlatformBlob",
                    reason: "platform blob arrays must contain only u8 values",
                });
            }
            let Some(encoded) = values.get(8..12) else {
                return Err(MediaInspectionError::UnsupportedLayout {
                    family: "Texture2D",
                    layout: "Nintendo Switch texture with a short m_PlatformBlob",
                });
            };
            for (output, value) in exponent.iter_mut().zip(encoded) {
                *output = value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or(MediaInspectionError::InvalidDescriptor {
                        field: "m_PlatformBlob",
                        reason: "platform blob arrays must contain only u8 values",
                    })?;
            }
        }
        _ => {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_PlatformBlob",
                reason: "platform blob must be bytes or a byte array",
            });
        }
    }
    let block_height_log2 = u32::from_le_bytes(exponent);
    if block_height_log2 > MAX_SWITCH_BLOCK_HEIGHT_LOG2 {
        return Err(MediaInspectionError::UnsupportedLayout {
            family: "Texture2D",
            layout: "Nintendo Switch block height exceeds the proven domain",
        });
    }
    Ok(block_height_log2)
}

const fn switch_storage_block(format: TextureFormat) -> Option<(u32, u32)> {
    match format {
        TextureFormat::Alpha8 => Some((16, 1)),
        TextureFormat::ARGB4444 | TextureFormat::RGBA4444 | TextureFormat::RGB565 => Some((8, 1)),
        TextureFormat::RGBA32 | TextureFormat::ARGB32 | TextureFormat::BGRA32 => Some((4, 1)),
        TextureFormat::DXT1 | TextureFormat::BC4 => Some((8, 4)),
        TextureFormat::DXT5 | TextureFormat::BC5 | TextureFormat::BC7 => Some((4, 4)),
        TextureFormat::ASTC_RGBA_4x4 => Some((4, 4)),
        TextureFormat::ASTC_RGBA_6x6 => Some((6, 6)),
        TextureFormat::ASTC_RGBA_8x8 => Some((8, 8)),
        _ => None,
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

    fn inspect(object: &UnityObject) -> Result<Texture2DLayout<'_>, MediaInspectionError> {
        Texture2DLayout::inspect_for_test(object, Some(5))
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
        let layout = inspect(&texture).unwrap();
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
            inspect(&object(properties)),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "image_data",
                ..
            })
        ));

        assert_eq!(
            inspect(&object(IndexMap::new())),
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
            inspect(&object(properties)),
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
            inspect(&object(dual)),
            Err(MediaInspectionError::AmbiguousPayload)
        );

        let mut overflow = base();
        overflow.insert(
            "m_StreamData".to_owned(),
            stream("archive:/CAB-a/CAB-a.resS", u64::MAX, 1),
        );
        assert_eq!(
            inspect(&object(overflow)),
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
            inspect(&object(properties)),
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
            inspect(&object(properties)),
            Err(MediaInspectionError::UnsupportedLayout {
                family: "Texture2D",
                layout: "legacy m_MipMap mip layout",
            })
        );

        let mut malformed = base();
        malformed.shift_remove("m_MipCount");
        malformed.insert("m_MipMap".to_owned(), UnityValue::Float(0.0));
        assert!(matches!(
            inspect(&object(malformed)),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "m_MipMap",
                ..
            })
        ));
    }

    #[test]
    fn switch_platform_requires_complete_blob_evidence() {
        let mut missing = base();
        missing.insert("image_data".to_owned(), UnityValue::Bytes(vec![0; 16]));
        assert!(matches!(
            Texture2DLayout::inspect_for_test(&object(missing), Some(38)),
            Err(MediaInspectionError::UnsupportedLayout {
                family: "Texture2D",
                layout: "Nintendo Switch texture without m_PlatformBlob evidence",
            })
        ));

        let mut short = base();
        short.insert("m_PlatformBlob".to_owned(), UnityValue::Bytes(vec![0; 11]));
        short.insert("image_data".to_owned(), UnityValue::Bytes(vec![0; 16]));
        assert!(matches!(
            Texture2DLayout::inspect_for_test(&object(short), Some(38)),
            Err(MediaInspectionError::UnsupportedLayout {
                family: "Texture2D",
                layout: "Nintendo Switch texture with a short m_PlatformBlob",
            })
        ));
    }

    #[test]
    fn switch_block_linear_edges_fail_closed() {
        let mut properties = base();
        let mut platform_blob = vec![0_u8; 12];
        platform_blob[8..12].copy_from_slice(&1_u32.to_le_bytes());
        properties.insert(
            "m_PlatformBlob".to_owned(),
            UnityValue::Bytes(platform_blob),
        );
        properties.insert("image_data".to_owned(), UnityValue::Bytes(vec![0; 16]));

        assert_eq!(
            Texture2DLayout::inspect_for_test(&object(properties), Some(38)),
            Err(MediaInspectionError::UnsupportedLayout {
                family: "Texture2D",
                layout: "Nintendo Switch block-linear edge padding",
            })
        );
    }

    #[test]
    fn absent_unknown_and_switch_2_platforms_fail_closed() {
        for (target, layout) in [
            (None, "SerializedFile target-platform metadata is absent"),
            (Some(3716), "unproven target-platform texture storage"),
            (Some(48), "Nintendo Switch 2 texture storage"),
        ] {
            let mut properties = base();
            properties.insert("image_data".to_owned(), UnityValue::Bytes(vec![0; 16]));
            assert_eq!(
                Texture2DLayout::inspect_for_test(&object(properties), target),
                Err(MediaInspectionError::UnsupportedLayout {
                    family: "Texture2D",
                    layout,
                })
            );
        }
    }
}
