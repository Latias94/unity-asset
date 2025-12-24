//! AudioClip converter and processor
//!
//! This module provides the main conversion logic for Unity AudioClip objects.
//! Inspired by UnityPy/export/AudioClipConverter.py

use super::formats::AudioCompressionFormat;
use super::types::{AudioClip, AudioClipMeta, StreamingInfo};
use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::unity_version::UnityVersion;
use unity_asset_core::UnityValue;

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
        fn as_u64(v: &UnityValue) -> Option<u64> {
            v.as_i64().and_then(|n| u64::try_from(n).ok())
        }
        fn as_u32(v: &UnityValue) -> Option<u32> {
            v.as_i64().and_then(|n| u32::try_from(n).ok())
        }
        fn as_f32(v: &UnityValue) -> Option<f32> {
            v.as_f64().map(|n| n as f32)
        }

        let props = obj.class.properties();

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

        let mut clip = AudioClip::default();
        clip.name = name;
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

        clip.ambisonic = props.get("m_Ambisonic").and_then(|v| v.as_bool());

        // Embedded audio bytes: `m_AudioData: List[int]`
        if let Some(UnityValue::Array(items)) = props.get("m_AudioData") {
            let mut bytes = Vec::with_capacity(items.len());
            for item in items {
                if let Some(n) = item.as_i64() {
                    bytes.push((n as i64).clamp(0, 255) as u8);
                }
            }
            clip.data = bytes;
        }

        // Streamed resource info: `m_Resource: { m_Source, m_Offset, m_Size }`
        if let Some(UnityValue::Object(res)) = props.get("m_Resource") {
            let source = res
                .get("m_Source")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let offset = res.get("m_Offset").and_then(as_u64).unwrap_or(0);
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

            if let Ok(source) = reader.read_aligned_string() {
                let looks_like_source = source.is_empty()
                    || source.contains("archive:/")
                    || source.contains('/')
                    || source.contains('\\')
                    || source.ends_with(".resS")
                    || source.ends_with(".resource");
                if looks_like_source {
                    resource_source = source;
                    resource_offset = reader.read_u64().unwrap_or(0);
                    resource_size = reader.read_u32().unwrap_or(0);
                    let _ = reader.align();
                    parsed_resource = true;
                }
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

            if !compression_format_read && reader.remaining() >= 4 {
                if let Ok(val) = reader.read_i32() {
                    if (-1..=25).contains(&val) {
                        compression_format = AudioCompressionFormat::from(val);
                    }
                }
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

    /// Load streaming data from external file
    pub fn load_streaming_data(&self, clip: &AudioClip) -> Result<Vec<u8>> {
        if clip.stream_info.path.is_empty() {
            return Err(BinaryError::invalid_data("No streaming path specified"));
        }

        // Try to read from the streaming file
        use std::fs;
        use std::path::Path;

        let stream_path = Path::new(&clip.stream_info.path);

        // Try different possible locations for the streaming file
        let possible_paths = [
            stream_path.to_path_buf(),
            Path::new("StreamingAssets").join(stream_path),
            Path::new("..").join(stream_path),
        ];

        for path in &possible_paths {
            if path.exists() {
                match fs::File::open(path) {
                    Ok(mut file) => {
                        use std::io::{Read, Seek, SeekFrom};

                        // Seek to the specified offset
                        if file.seek(SeekFrom::Start(clip.stream_info.offset)).is_err() {
                            continue; // Try next path
                        }

                        // Read the specified amount of data
                        let mut buffer = vec![0u8; clip.stream_info.size as usize];
                        match file.read_exact(&mut buffer) {
                            Ok(_) => return Ok(buffer),
                            Err(_) => continue, // Try next path
                        }
                    }
                    Err(_) => continue, // Try next path
                }
            }
        }

        Err(BinaryError::generic(format!(
            "Could not load streaming data from: {}",
            clip.stream_info.path
        )))
    }

    /// Get audio data (either embedded or streamed)
    pub fn get_audio_data(&self, clip: &AudioClip) -> Result<Vec<u8>> {
        if !clip.data.is_empty() {
            // Use embedded data
            Ok(clip.data.clone())
        } else if clip.is_streamed() {
            // Load streaming data
            self.load_streaming_data(clip)
        } else {
            Err(BinaryError::invalid_data("No audio data available"))
        }
    }
}

// Legacy compatibility - alias for the old processor name
pub type AudioClipProcessor = AudioClipConverter;
