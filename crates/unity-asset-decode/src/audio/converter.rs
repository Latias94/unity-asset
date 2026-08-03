//! Compatibility adapter from strict AudioClip TypeTree inspection to the legacy owned model.

use super::formats::AudioCompressionFormat;
use super::inspection::AudioClipLayout;
use super::types::{AudioClip, AudioClipMeta, StreamingInfo};
use crate::media::MediaPayloadRef;
use unity_asset_binary::object::UnityObject;
use unity_asset_binary::unity_version::UnityVersion;
use unity_asset_binary::{BinaryError, Result};
use unity_asset_core::UnityValue;

/// Legacy owned AudioClip adapter.
///
/// New code should use [`AudioClipLayout`] and prepared media writers directly.
pub struct AudioClipConverter;

impl AudioClipConverter {
    /// Retains the former constructor shape without using guessed version layouts.
    #[must_use]
    pub fn new(_version: UnityVersion) -> Self {
        Self
    }

    /// Converts a strictly inspected TypeTree-backed object into the legacy owned model.
    pub fn from_unity_object(&self, object: &UnityObject) -> Result<AudioClip> {
        let layout = AudioClipLayout::inspect(object)
            .map_err(|error| BinaryError::invalid_data(error.to_string()))?;
        let properties = object.as_unity_class().properties();
        let name = properties
            .get("m_Name")
            .and_then(UnityValue::as_str)
            .expect("strict AudioClip inspection validates m_Name")
            .to_owned();

        let mut clip = AudioClip {
            name,
            meta: AudioClipMeta::Modern {
                load_type: optional_i32(properties.get("m_LoadType"), 0),
                channels: optional_i32(properties.get("m_Channels"), 2),
                frequency: optional_i32(properties.get("m_Frequency"), 44_100),
                bits_per_sample: optional_i32(properties.get("m_BitsPerSample"), 16),
                length: optional_f32(properties.get("m_Length"), 0.0),
                is_tracker_format: optional_bool(properties.get("m_IsTrackerFormat"), false),
                subsound_index: layout.subsound_index(),
                preload_audio_data: optional_bool(properties.get("m_PreloadAudioData"), true),
                load_in_background: optional_bool(properties.get("m_LoadInBackground"), false),
                legacy_3d: optional_bool(properties.get("m_Legacy3D"), false),
                compression_format: layout.compression_format(),
            },
            ambisonic: properties.get("m_Ambisonic").and_then(UnityValue::as_bool),
            ..AudioClip::default()
        };

        match layout.payload() {
            MediaPayloadRef::Embedded(_) => {
                clip.data =
                    owned_bytes(properties.get("m_AudioData").expect(
                        "strict AudioClip inspection validates embedded payload presence",
                    ))?;
                clip.size = u64::try_from(clip.data.len())
                    .map_err(|_| BinaryError::invalid_data("audio byte length exceeds u64"))?;
            }
            MediaPayloadRef::Streamed(stream) => {
                let size = u32::try_from(stream.size()).map_err(|_| {
                    BinaryError::invalid_data("legacy AudioClip stream size exceeds u32")
                })?;
                clip.stream_info = StreamingInfo {
                    offset: stream.offset(),
                    size,
                    path: stream.path().to_owned(),
                };
                clip.source = Some(stream.path().to_owned());
                clip.offset = stream.offset();
                clip.size = stream.size();
            }
        }
        Ok(clip)
    }

    #[must_use]
    pub fn supported_formats(&self) -> Vec<AudioCompressionFormat> {
        vec![
            AudioCompressionFormat::PCM,
            AudioCompressionFormat::Vorbis,
            AudioCompressionFormat::ADPCM,
            AudioCompressionFormat::MP3,
            AudioCompressionFormat::AAC,
        ]
    }

    #[must_use]
    pub fn can_process(&self, format: AudioCompressionFormat) -> bool {
        self.supported_formats().contains(&format)
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
                        BinaryError::invalid_data("AudioClip byte array contains a non-u8 value")
                    })
            })
            .collect(),
        _ => Err(BinaryError::invalid_data(
            "AudioClip embedded payload is not bytes",
        )),
    }
}

fn optional_i32(value: Option<&UnityValue>, default: i32) -> i32 {
    value
        .and_then(UnityValue::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(default)
}

fn optional_f32(value: Option<&UnityValue>, default: f32) -> f32 {
    value
        .and_then(UnityValue::as_f64)
        .map(|value| value as f32)
        .unwrap_or(default)
}

fn optional_bool(value: Option<&UnityValue>, default: bool) -> bool {
    value.and_then(UnityValue::as_bool).unwrap_or(default)
}
