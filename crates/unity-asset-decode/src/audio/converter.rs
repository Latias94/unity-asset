//! AudioClip converter and processor
//!
//! This module provides the main conversion logic for Unity AudioClip objects.
//! Inspired by UnityPy/export/AudioClipConverter.py

use super::formats::AudioCompressionFormat;
use super::types::{AudioClip, AudioClipMeta, StreamingInfo};
use crate::error::{BinaryError, Result};
use crate::media::{MediaPayloadRef, StreamDataRef, is_plausible_stream_path};
use crate::object::UnityObject;
use crate::reader::{BinaryReader, ByteOrder};
use crate::unity_version::UnityVersion;
use unity_asset_core::UnityValue;

/// Allocation-free AudioClip metadata used by planners and inventory tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioClipLayout<'a> {
    compression_format: AudioCompressionFormat,
    payload: MediaPayloadRef<'a>,
}

impl<'a> AudioClipLayout<'a> {
    /// Inspects an AudioClip without cloning its embedded or streamed media.
    pub fn inspect(obj: &'a UnityObject, version: &UnityVersion) -> Result<Self> {
        inspect_audio_typetree(obj)
            .map_or_else(|| inspect_audio_binary(obj.raw_data(), version), Ok)
    }

    #[must_use]
    pub const fn compression_format(self) -> AudioCompressionFormat {
        self.compression_format
    }

    #[must_use]
    pub const fn payload(self) -> MediaPayloadRef<'a> {
        self.payload
    }
}

/// Main audio converter
///
/// This struct handles the conversion of Unity objects to AudioClip structures
/// and provides methods for processing audio data.
pub struct AudioClipConverter {
    version: UnityVersion,
}

impl AudioClipConverter {
    /// Create a new AudioClip converter
    pub fn new(version: UnityVersion) -> Self {
        Self { version }
    }

    /// Convert Unity object to AudioClip
    ///
    /// This method extracts audio data from a Unity object and creates
    /// an AudioClip structure with all necessary metadata.
    pub fn from_unity_object(&self, obj: &UnityObject) -> Result<AudioClip> {
        // Prefer TypeTree when available; this is much more reliable for streamed clips.
        if let Ok(clip) = self.try_parse_typetree(obj) {
            return Ok(clip);
        }

        // Fallback: raw binary parsing (best-effort; version-dependent).
        self.parse_binary_data(obj.raw_data())
    }

    fn try_parse_typetree(&self, obj: &UnityObject) -> Result<AudioClip> {
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
            .ok_or_else(|| BinaryError::invalid_data("AudioClip missing m_Name"))?
            .to_string();

        let channels = props.get("m_Channels").and_then(as_i32).unwrap_or(2);
        let frequency = props.get("m_Frequency").and_then(as_i32).unwrap_or(44100);
        let bits_per_sample = props.get("m_BitsPerSample").and_then(as_i32).unwrap_or(16);
        let length = props.get("m_Length").and_then(as_f32).unwrap_or(0.0);

        let load_type = props.get("m_LoadType").and_then(as_i32).unwrap_or(0);
        let is_tracker_format = props
            .get("m_IsTrackerFormat")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let subsound_index = props.get("m_SubsoundIndex").and_then(as_i32).unwrap_or(0);
        let preload_audio_data = props
            .get("m_PreloadAudioData")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let load_in_background = props
            .get("m_LoadInBackground")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let legacy_3d = props
            .get("m_Legacy3D")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let compression_format_val = props
            .get("m_CompressionFormat")
            .and_then(as_i32)
            .unwrap_or(0);
        let compression_format = AudioCompressionFormat::from(compression_format_val);

        let mut clip = AudioClip {
            name,
            meta: AudioClipMeta::Modern {
                load_type,
                channels,
                frequency,
                bits_per_sample,
                length,
                is_tracker_format,
                subsound_index,
                preload_audio_data,
                load_in_background,
                legacy_3d,
                compression_format,
            },
            ambisonic: props.get("m_Ambisonic").and_then(|v| v.as_bool()),
            ..Default::default()
        };

        // Embedded audio bytes: `m_AudioData: List[int]` / `Bytes`
        if let Some(v) = props.get("m_AudioData") {
            match v {
                UnityValue::Bytes(b) => clip.data = b.clone(),
                UnityValue::Array(items) => {
                    let mut bytes = Vec::with_capacity(items.len());
                    for item in items {
                        if let Some(n) = item.as_i64() {
                            bytes.push(n.clamp(0, 255) as u8);
                        }
                    }
                    clip.data = bytes;
                }
                _ => {}
            }
        }

        // Streamed resource info: `m_Resource: { m_Source, m_Offset, m_Size }`
        if let Some(UnityValue::Object(res)) = props.get("m_Resource") {
            let source = res
                .get("m_Source")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let offset = res
                .get("m_Offset")
                .and_then(UnityValue::as_u64)
                .unwrap_or(0);
            let size = res.get("m_Size").and_then(as_u32).unwrap_or(0);

            if !source.is_empty() && size > 0 {
                clip.stream_info = StreamingInfo {
                    offset,
                    size,
                    path: source.clone(),
                };
                clip.source = Some(source);
                clip.offset = offset;
                clip.size = size as u64;
            }
        }

        if clip.data.is_empty() && !clip.is_streamed() {
            return Err(BinaryError::invalid_data(
                "AudioClip typetree did not contain audio bytes or stream resource info",
            ));
        }

        Ok(clip)
    }

    /// Parse AudioClip from raw binary data (simplified version)
    #[allow(clippy::field_reassign_with_default)]
    fn parse_binary_data(&self, data: &[u8]) -> Result<AudioClip> {
        if data.is_empty() {
            return Err(BinaryError::invalid_data("Empty audio data"));
        }

        let mut reader = crate::reader::BinaryReader::new(data, crate::reader::ByteOrder::Little);
        let mut clip = AudioClip::default();

        // Read name first
        clip.name = reader
            .read_aligned_string()
            .unwrap_or_else(|_| "UnknownAudio".to_string());

        // Read metadata based on Unity version
        if self.version.major < 5 {
            // Legacy format (Unity < 5.0)
            let format = reader.read_i32().unwrap_or(0);
            let type_ = reader.read_i32().unwrap_or(0);
            let is_3d = reader.read_bool().unwrap_or(false);
            let use_hardware = reader.read_bool().unwrap_or(false);

            clip.meta = AudioClipMeta::Legacy {
                format,
                type_,
                is_3d,
                use_hardware,
            };
        } else {
            // Modern format (Unity >= 5.0)
            let load_type = reader.read_i32().unwrap_or(0);
            let channels = reader.read_i32().unwrap_or(2);
            let frequency = reader.read_i32().unwrap_or(44100);
            let bits_per_sample = reader.read_i32().unwrap_or(16);
            let length = reader.read_f32().unwrap_or(0.0);
            let is_tracker_format = reader.read_bool().unwrap_or(false);
            let _ = reader.align();
            let subsound_index = reader.read_i32().unwrap_or(0);
            let preload_audio_data = reader.read_bool().unwrap_or(true);
            let load_in_background = reader.read_bool().unwrap_or(false);
            let legacy_3d = reader.read_bool().unwrap_or(false);
            let _ = reader.align();

            let mut compression_format = AudioCompressionFormat::Unknown;
            let mut compression_format_read = false;

            // Some Unity versions store `m_CompressionFormat` before `m_Resource`, while others
            // store it at the end. Try to parse `m_Resource` first (string -> offset -> size);
            // if that does not look plausible, fall back to reading `m_CompressionFormat` first.
            let mut resource_source = String::new();
            let mut resource_offset = 0u64;
            let mut resource_size = 0u32;

            let resource_pos = reader.position();
            let mut parsed_resource = false;

            if let Ok(source) = reader.read_aligned_string()
                && is_plausible_stream_path(&source)
            {
                resource_source = source;
                resource_offset = reader.read_u64().unwrap_or(0);
                resource_size = reader.read_u32().unwrap_or(0);
                let _ = reader.align();
                parsed_resource = true;
            }

            if !parsed_resource {
                let _ = reader.set_position(resource_pos);
                let compression_format_val = reader.read_i32().unwrap_or(-1);
                compression_format = AudioCompressionFormat::from(compression_format_val);
                compression_format_read = true;

                if self.version.major >= 2017 {
                    clip.ambisonic = reader.read_bool().ok();
                    let _ = reader.align();
                }

                resource_source = reader.read_aligned_string().unwrap_or_default();
                resource_offset = reader.read_u64().unwrap_or(0);
                resource_size = reader.read_u32().unwrap_or(0);
                let _ = reader.align();
            }

            if !resource_source.is_empty() && resource_size > 0 {
                clip.stream_info = StreamingInfo {
                    offset: resource_offset,
                    size: resource_size,
                    path: resource_source.clone(),
                };
                clip.source = Some(resource_source);
                clip.offset = resource_offset;
                clip.size = resource_size as u64;
            }

            // Read audio data size and data
            let data_size = reader.read_u32().unwrap_or(0);
            if data_size > 0 && reader.remaining() >= data_size as usize {
                clip.data = reader.read_bytes(data_size as usize).unwrap_or_default();
            } else if !clip.is_streamed() && reader.remaining() > 0 {
                // Fallback: take all remaining data (only for non-streamed clips).
                let remaining_data = reader.read_remaining();
                clip.data = remaining_data.to_vec();
            }

            if !compression_format_read
                && reader.remaining() >= 4
                && let Ok(val) = reader.read_i32()
                && (-1..=25).contains(&val)
            {
                compression_format = AudioCompressionFormat::from(val);
            }

            clip.meta = AudioClipMeta::Modern {
                load_type,
                channels,
                frequency,
                bits_per_sample,
                length,
                is_tracker_format,
                subsound_index,
                preload_audio_data,
                load_in_background,
                legacy_3d,
                compression_format,
            };
        }

        clip.size = if clip.is_streamed() {
            clip.stream_info.size as u64
        } else {
            clip.data.len() as u64
        };

        Ok(clip)
    }

    /// Get supported formats for this Unity version
    pub fn supported_formats(&self) -> Vec<AudioCompressionFormat> {
        let mut formats = vec![
            AudioCompressionFormat::PCM,
            AudioCompressionFormat::Vorbis,
            AudioCompressionFormat::ADPCM,
        ];

        // Add formats based on Unity version
        if self.version.major >= 4 {
            formats.push(AudioCompressionFormat::MP3);
        }

        if self.version.major >= 5 {
            formats.push(AudioCompressionFormat::AAC);
        }

        // Platform-specific formats (usually not supported for decoding)
        // formats.push(AudioCompressionFormat::VAG);
        // formats.push(AudioCompressionFormat::XMA);
        // formats.push(AudioCompressionFormat::ATRAC9);

        formats
    }

    /// Check if a format can be processed
    pub fn can_process(&self, format: AudioCompressionFormat) -> bool {
        self.supported_formats().contains(&format)
    }
}

fn inspect_audio_typetree(obj: &UnityObject) -> Option<AudioClipLayout<'_>> {
    let props = obj.as_unity_class().properties();
    props.get("m_Name").and_then(UnityValue::as_str)?;

    let compression_format = props
        .get("m_CompressionFormat")
        .and_then(UnityValue::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(0);
    let compression_format = AudioCompressionFormat::from(compression_format);

    let embedded_byte_len = match props.get("m_AudioData") {
        Some(UnityValue::Bytes(bytes)) => bytes.len(),
        Some(UnityValue::Array(items)) => {
            items.iter().filter(|item| item.as_i64().is_some()).count()
        }
        _ => 0,
    };
    let stream = props
        .get("m_Resource")
        .and_then(|value| match value {
            UnityValue::Object(resource) => Some(resource),
            _ => None,
        })
        .and_then(|resource| {
            let path = resource.get("m_Source").and_then(UnityValue::as_str)?;
            let offset = resource
                .get("m_Offset")
                .and_then(UnityValue::as_u64)
                .unwrap_or(0);
            let size = resource
                .get("m_Size")
                .and_then(UnityValue::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            StreamDataRef::new(path, offset, size)
        });

    let payload = MediaPayloadRef::select(embedded_byte_len, stream)?;
    Some(AudioClipLayout {
        compression_format,
        payload,
    })
}

fn inspect_audio_binary<'a>(data: &'a [u8], version: &UnityVersion) -> Result<AudioClipLayout<'a>> {
    if data.is_empty() {
        return Err(BinaryError::invalid_data("Empty audio data"));
    }

    let mut reader = BinaryReader::new(data, ByteOrder::Little);
    reader.read_aligned_string_ref()?;
    if version.major < 5 {
        return Err(BinaryError::unsupported_version(
            "allocation-free AudioClip inspection requires Unity 5 or newer",
        ));
    }

    reader.read_i32()?;
    reader.read_i32()?;
    reader.read_i32()?;
    reader.read_i32()?;
    reader.read_f32()?;
    reader.read_bool()?;
    reader.align()?;
    reader.read_i32()?;
    reader.read_bool()?;
    reader.read_bool()?;
    reader.read_bool()?;
    reader.align()?;

    let resource_position = reader.position();
    let mut compression_format = AudioCompressionFormat::Unknown;
    let mut compression_format_read = false;
    let resource = if let Some(resource) = try_read_path_first_stream(&mut reader) {
        resource
    } else {
        reader.set_position(resource_position)?;
        compression_format = AudioCompressionFormat::from(reader.read_i32()?);
        compression_format_read = true;
        if version.major >= 2017 {
            reader.read_bool()?;
            reader.align()?;
        }
        let path = reader.read_aligned_string_ref()?;
        let offset = reader.read_u64()?;
        let size = reader.read_u32()?;
        let _ = reader.align();
        (path, offset, size)
    };
    let stream = StreamDataRef::new(resource.0, resource.1, resource.2);

    let declared_size = reader.read_u32().unwrap_or(0);
    let embedded_byte_len = if declared_size > 0
        && reader.remaining() >= usize::try_from(declared_size).unwrap_or(usize::MAX)
    {
        let size = declared_size as usize;
        reader.skip_bytes(size)?;
        size
    } else if stream.is_none() {
        reader.remaining()
    } else {
        0
    };

    if !compression_format_read
        && reader.remaining() >= 4
        && let Ok(value) = reader.read_i32()
        && (-1..=25).contains(&value)
    {
        compression_format = AudioCompressionFormat::from(value);
    }

    let payload = MediaPayloadRef::select(embedded_byte_len, stream).ok_or_else(|| {
        BinaryError::invalid_data("AudioClip did not contain embedded bytes or stream data")
    })?;
    Ok(AudioClipLayout {
        compression_format,
        payload,
    })
}

fn try_read_path_first_stream<'a>(reader: &mut BinaryReader<'a>) -> Option<(&'a str, u64, u32)> {
    let path = reader.read_aligned_string_ref().ok()?;
    if !is_plausible_stream_path(path) {
        return None;
    }
    let offset = reader.read_u64().ok()?;
    let size = reader.read_u32().ok()?;
    let _ = reader.align();
    Some((path, offset, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{ObjectInfo, class_ids};
    use indexmap::IndexMap;
    use unity_asset_core::UnityClass;

    fn test_version() -> UnityVersion {
        UnityVersion::parse_version("2020.3.12f1").unwrap()
    }

    #[test]
    fn typetree_stream_offset_preserves_unsigned_range() {
        let class = UnityClass::with_properties(
            class_ids::AUDIO_CLIP,
            "AudioClip".to_string(),
            "1".to_string(),
            IndexMap::from([
                ("m_Name".to_string(), UnityValue::String("Clip".to_string())),
                (
                    "m_Resource".to_string(),
                    UnityValue::Object(IndexMap::from([
                        (
                            "m_Source".to_string(),
                            UnityValue::String("archive:/CAB-a/CAB-a.resS".to_string()),
                        ),
                        ("m_Offset".to_string(), UnityValue::from(u64::MAX)),
                        ("m_Size".to_string(), UnityValue::Integer(1)),
                    ])),
                ),
            ]),
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::AUDIO_CLIP)
            .expect("valid standalone audio object");
        let object = UnityObject::from_info_and_class(info, class);

        let version = test_version();
        let clip = AudioClipConverter::new(version.clone())
            .from_unity_object(&object)
            .unwrap();

        assert_eq!(clip.stream_info.offset, u64::MAX);

        let layout = AudioClipLayout::inspect(&object, &version).unwrap();
        assert_eq!(layout.payload().stream().unwrap().offset(), u64::MAX);
        assert_eq!(layout.compression_format(), clip.compression_format());
    }

    #[test]
    fn typetree_layout_counts_embedded_audio_without_owning_it() {
        let class = UnityClass::with_properties(
            class_ids::AUDIO_CLIP,
            "AudioClip".to_string(),
            "1".to_string(),
            IndexMap::from([
                ("m_Name".to_string(), UnityValue::String("Clip".to_string())),
                (
                    "m_CompressionFormat".to_string(),
                    UnityValue::Integer(AudioCompressionFormat::Vorbis as i64),
                ),
                (
                    "m_AudioData".to_string(),
                    UnityValue::Bytes(vec![7; 1024 * 1024]),
                ),
            ]),
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::AUDIO_CLIP)
            .expect("valid standalone audio object");
        let object = UnityObject::from_info_and_class(info, class);

        let version = test_version();
        let layout = AudioClipLayout::inspect(&object, &version).unwrap();

        assert_eq!(
            layout.payload(),
            MediaPayloadRef::Embedded {
                byte_len: 1024 * 1024
            }
        );
        assert_eq!(layout.compression_format(), AudioCompressionFormat::Vorbis);
    }

    #[test]
    fn raw_layout_borrows_stream_path() {
        let path = "archive:/CAB-a/CAB-a.resS";
        let mut data = Vec::new();
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&2_i32.to_le_bytes());
        data.extend_from_slice(&44_100_i32.to_le_bytes());
        data.extend_from_slice(&16_i32.to_le_bytes());
        data.extend_from_slice(&0_f32.to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(&0_i32.to_le_bytes());
        data.extend_from_slice(&[1, 0, 0, 0]);
        data.extend_from_slice(&(path.len() as i32).to_le_bytes());
        let path_offset = data.len();
        data.extend_from_slice(path.as_bytes());
        while data.len() % 4 != 0 {
            data.push(0);
        }
        data.extend_from_slice(&17_u64.to_le_bytes());
        data.extend_from_slice(&23_u32.to_le_bytes());
        while data.len() % 4 != 0 {
            data.push(0);
        }
        data.extend_from_slice(&0_u32.to_le_bytes());
        data.extend_from_slice(&(AudioCompressionFormat::Vorbis as i32).to_le_bytes());

        let layout = inspect_audio_binary(&data, &test_version()).unwrap();
        let stream = layout.payload().stream().unwrap();

        assert_eq!(stream.path(), path);
        assert_eq!(stream.path().as_ptr(), data[path_offset..].as_ptr());
        assert_eq!(stream.offset(), 17);
        assert_eq!(stream.size(), 23);
        assert_eq!(layout.compression_format(), AudioCompressionFormat::Vorbis);
    }
}
