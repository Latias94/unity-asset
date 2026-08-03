//! Strict AudioClip TypeTree inspection.

use indexmap::IndexMap;
use unity_asset_core::UnityValue;

use super::formats::AudioCompressionFormat;
use crate::media::{
    EmbeddedMediaRef, MediaInspectionError, MediaPayloadRef, classify_audio_clip_resource,
};
use unity_asset_binary::asset::class_ids;
use unity_asset_binary::object::UnityObject;

/// Allocation-free AudioClip metadata used by preparation and planners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioClipLayout<'a> {
    compression_format: AudioCompressionFormat,
    subsound_index: i32,
    payload: MediaPayloadRef<'a>,
}

impl<'a> AudioClipLayout<'a> {
    /// Inspects one AudioClip using only materialized TypeTree evidence.
    pub fn inspect(object: &'a UnityObject) -> Result<Self, MediaInspectionError> {
        if object.class_id() != class_ids::AUDIO_CLIP {
            return Err(MediaInspectionError::NotApplicable {
                expected: class_ids::AUDIO_CLIP,
                actual: object.class_id(),
            });
        }
        let properties = object.as_unity_class().properties();
        if properties.is_empty() {
            return Err(MediaInspectionError::TypeTreeUnavailable);
        }

        required_string(properties, "m_Name")?;
        let Some(compression_format) = optional_i32(properties, "m_CompressionFormat")? else {
            return Err(MediaInspectionError::UnsupportedLayout {
                family: "AudioClip",
                layout: "AudioClip without m_CompressionFormat",
            });
        };
        let compression_format = AudioCompressionFormat::from(compression_format);
        if compression_format == AudioCompressionFormat::Unknown {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_CompressionFormat",
                reason: "audio compression format is unknown",
            });
        }
        let Some(subsound_index) = optional_i32(properties, "m_SubsoundIndex")? else {
            return Err(MediaInspectionError::UnsupportedLayout {
                family: "AudioClip",
                layout: "AudioClip without m_SubsoundIndex",
            });
        };
        if subsound_index < 0 {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_SubsoundIndex",
                reason: "audio subsound index must not be negative",
            });
        }

        let embedded = embedded_payload(properties.get("m_AudioData"))?;
        let stream = classify_audio_clip_resource(properties)?.map(|resource| resource.stream());
        let payload = MediaPayloadRef::classify(embedded, stream)?;

        Ok(Self {
            compression_format,
            subsound_index,
            payload,
        })
    }

    #[must_use]
    pub const fn compression_format(self) -> AudioCompressionFormat {
        self.compression_format
    }

    #[must_use]
    pub const fn subsound_index(self) -> i32 {
        self.subsound_index
    }

    #[must_use]
    pub const fn payload(self) -> MediaPayloadRef<'a> {
        self.payload
    }
}

fn embedded_payload(
    value: Option<&UnityValue>,
) -> Result<Option<EmbeddedMediaRef<'_>>, MediaInspectionError> {
    let Some(value) = value else {
        return Ok(None);
    };
    EmbeddedMediaRef::inspect(
        value,
        "m_AudioData",
        "audio byte arrays must contain only u8 values",
        "embedded audio data must be bytes or a byte array",
    )
    .map(Some)
}

fn required_string<'a>(
    fields: &'a IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<&'a str, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_str)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be a string",
        })
}

fn optional_i32(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<Option<i32>, MediaInspectionError> {
    let Some(value) = fields.get(field) else {
        return Ok(None);
    };
    value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .map(Some)
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
            class_ids::AUDIO_CLIP,
            "AudioClip".to_owned(),
            "1".to_owned(),
            properties,
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::AUDIO_CLIP).unwrap();
        UnityObject::from_info_and_class(info, class)
    }

    fn base() -> IndexMap<String, UnityValue> {
        IndexMap::from([
            ("m_Name".to_owned(), UnityValue::String("Clip".to_owned())),
            ("m_CompressionFormat".to_owned(), UnityValue::Integer(1)),
            ("m_SubsoundIndex".to_owned(), UnityValue::Integer(0)),
        ])
    }

    fn stream(path: &str, offset: u64, size: u64) -> UnityValue {
        UnityValue::Object(IndexMap::from([
            ("m_Source".to_owned(), UnityValue::String(path.to_owned())),
            ("m_Offset".to_owned(), UnityValue::from(offset)),
            ("m_Size".to_owned(), UnityValue::from(size)),
        ]))
    }

    #[test]
    fn typetree_requires_one_valid_payload() {
        let mut properties = base();
        properties.insert("m_AudioData".to_owned(), UnityValue::Bytes(vec![1, 2]));
        let embedded = object(properties);
        assert_eq!(
            AudioClipLayout::inspect(&embedded)
                .unwrap()
                .payload()
                .embedded_byte_len(),
            Some(2)
        );

        let mut properties = base();
        properties.insert(
            "m_Resource".to_owned(),
            stream("archive:/CAB-a/CAB-a.resS", 4, 8),
        );
        let streamed = object(properties);
        let selected = AudioClipLayout::inspect(&streamed)
            .unwrap()
            .payload()
            .stream()
            .unwrap();
        assert_eq!((selected.offset(), selected.size()), (4, 8));
    }

    #[test]
    fn valid_primary_stream_ignores_malformed_compatibility_field() {
        let mut properties = base();
        properties.insert(
            "m_Resource".to_owned(),
            stream("archive:/CAB-a/CAB-a.resS", 4, 8),
        );
        properties.insert("m_StreamData".to_owned(), UnityValue::Float(1.0));

        let audio_clip = object(properties);
        let selected = AudioClipLayout::inspect(&audio_clip)
            .unwrap()
            .payload()
            .stream()
            .unwrap();

        assert_eq!((selected.offset(), selected.size()), (4, 8));
    }

    #[test]
    fn malformed_typetree_never_becomes_unavailable_or_raw() {
        let mut properties = base();
        properties.insert("m_AudioData".to_owned(), UnityValue::Float(1.0));
        let malformed = object(properties);

        assert!(matches!(
            AudioClipLayout::inspect(&malformed),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "m_AudioData",
                ..
            })
        ));

        let unavailable = object(IndexMap::new());
        assert_eq!(
            AudioClipLayout::inspect(&unavailable),
            Err(MediaInspectionError::TypeTreeUnavailable)
        );
    }

    #[test]
    fn absent_versioned_audio_fields_are_unsupported_layouts() {
        let mut missing_subsound = base();
        missing_subsound.shift_remove("m_SubsoundIndex");
        missing_subsound.insert("m_AudioData".to_owned(), UnityValue::Bytes(vec![1, 2]));
        assert_eq!(
            AudioClipLayout::inspect(&object(missing_subsound)),
            Err(MediaInspectionError::UnsupportedLayout {
                family: "AudioClip",
                layout: "AudioClip without m_SubsoundIndex",
            })
        );

        let mut legacy_format = base();
        legacy_format.shift_remove("m_CompressionFormat");
        legacy_format.insert("m_Format".to_owned(), UnityValue::Integer(1));
        legacy_format.insert("m_AudioData".to_owned(), UnityValue::Bytes(vec![1, 2]));
        assert_eq!(
            AudioClipLayout::inspect(&object(legacy_format)),
            Err(MediaInspectionError::UnsupportedLayout {
                family: "AudioClip",
                layout: "AudioClip without m_CompressionFormat",
            })
        );
    }

    #[test]
    fn simultaneous_payloads_and_overflowing_ranges_are_rejected() {
        let mut dual = base();
        dual.insert("m_AudioData".to_owned(), UnityValue::Bytes(vec![1]));
        dual.insert(
            "m_Resource".to_owned(),
            stream("archive:/CAB-a/CAB-a.resS", 0, 1),
        );
        assert_eq!(
            AudioClipLayout::inspect(&object(dual)),
            Err(MediaInspectionError::AmbiguousPayload)
        );

        let mut overflow = base();
        overflow.insert(
            "m_Resource".to_owned(),
            stream("archive:/CAB-a/CAB-a.resS", u64::MAX, 1),
        );
        assert_eq!(
            AudioClipLayout::inspect(&object(overflow)),
            Err(MediaInspectionError::StreamRangeOverflow {
                offset: u64::MAX,
                size: 1,
            })
        );
    }
}
