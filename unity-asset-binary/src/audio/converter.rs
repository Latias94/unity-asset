//! AudioClip converter and processor
//!
//! This module provides the main conversion logic for Unity AudioClip objects.
//! Inspired by UnityPy/export/AudioClipConverter.py

use super::formats::AudioCompressionFormat;
use super::types::{AudioClip, AudioClipMeta, StreamingInfo};
use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::unity_version::UnityVersion;

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
        // For now, use a simplified approach similar to the texture implementation
        // TODO: Implement proper TypeTree parsing when available
        self.from_binary_data(&obj.info.data)
    }

    /// Parse AudioClip from raw binary data (simplified version)
    fn from_binary_data(&self, data: &[u8]) -> Result<AudioClip> {
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
            let subsound_index = reader.read_i32().unwrap_or(0);
            let preload_audio_data = reader.read_bool().unwrap_or(true);
            let load_in_background = reader.read_bool().unwrap_or(false);
            let legacy_3d = reader.read_bool().unwrap_or(false);

            let compression_format_val = reader.read_i32().unwrap_or(0);
            let compression_format = AudioCompressionFormat::from(compression_format_val);

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

            // Extract ambisonic flag (Unity 2017+)
            if self.version.major >= 2017 {
                clip.ambisonic = reader.read_bool().ok();
            }
        }

        // Read streaming info
        let stream_offset = reader.read_u64().unwrap_or(0);
        let stream_size = reader.read_u32().unwrap_or(0);
        let stream_path = reader.read_aligned_string().unwrap_or_default();

        clip.stream_info = StreamingInfo {
            offset: stream_offset,
            size: stream_size,
            path: stream_path,
        };

        // Read audio data size and data
        let data_size = reader.read_u32().unwrap_or(0);
        if data_size > 0 && reader.remaining() >= data_size as usize {
            clip.data = reader.read_bytes(data_size as usize).unwrap_or_default();
        } else if reader.remaining() > 0 {
            // Fallback: take all remaining data
            let remaining_data = reader.read_remaining();
            clip.data = remaining_data.to_vec();
        }

        clip.size = clip.data.len() as u64;

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
                        if let Err(_) = file.seek(SeekFrom::Start(clip.stream_info.offset)) {
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
