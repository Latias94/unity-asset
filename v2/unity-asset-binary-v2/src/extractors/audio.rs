//! Async Audio Processing
//!
//! Provides async audio extraction and processing for Unity AudioClip assets.
//! Supports various audio formats with streaming decompression and async conversion.

use crate::async_compression::{AsyncDecompressor, UnityAsyncDecompressor};
use crate::binary_types::{AsyncBinaryData, AsyncBinaryReader};
use crate::stream_reader::AsyncStreamReader;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::task;
use unity_asset_core_v2::{AsyncUnityClass, Result, UnityAssetError, UnityValue};

#[cfg(feature = "audio")]
use symphonia::core::audio::{AudioBuffer, Signal};
#[cfg(feature = "audio")]
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
#[cfg(feature = "audio")]
use symphonia::core::formats::FormatOptions;
#[cfg(feature = "audio")]
use symphonia::core::io::MediaSourceStream;
#[cfg(feature = "audio")]
use symphonia::core::meta::MetadataOptions;
#[cfg(feature = "audio")]
use symphonia::core::probe::Hint;

/// Async audio processor configuration
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Maximum audio duration in seconds for safety
    pub max_duration_seconds: f32,
    /// Target sample rate for conversion
    pub target_sample_rate: u32,
    /// Target number of channels
    pub target_channels: u16,
    /// Output audio format
    pub output_format: AudioOutputFormat,
    /// Whether to normalize audio levels
    pub normalize_audio: bool,
    /// Maximum memory usage for audio processing
    pub max_memory_mb: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            max_duration_seconds: 300.0, // 5 minutes max
            target_sample_rate: 44100,
            target_channels: 2,
            output_format: AudioOutputFormat::Wav,
            normalize_audio: false,
            max_memory_mb: 256, // 256MB max for audio processing
        }
    }
}

/// Supported output audio formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioOutputFormat {
    Wav,
    Mp3,
    Ogg,
    Raw,
}

/// Unity audio formats and compression types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnityAudioFormat {
    PCM = 1,
    Vorbis = 2,
    ADPCM = 3,
    MP3 = 4,
    VAG = 5,
    HEVAG = 6,
    XMA = 7,
    AAC = 8,
    GCADPCM = 9,
    ATRAC9 = 10,
}

impl UnityAudioFormat {
    /// Create from Unity audio type ID
    pub fn from_id(id: i32) -> Option<Self> {
        match id {
            1 => Some(Self::PCM),
            2 => Some(Self::Vorbis),
            3 => Some(Self::ADPCM),
            4 => Some(Self::MP3),
            5 => Some(Self::VAG),
            6 => Some(Self::HEVAG),
            7 => Some(Self::XMA),
            8 => Some(Self::AAC),
            9 => Some(Self::GCADPCM),
            10 => Some(Self::ATRAC9),
            _ => None,
        }
    }

    /// Get format ID
    pub fn id(&self) -> i32 {
        *self as i32
    }

    /// Check if format requires decompression
    pub fn is_compressed(&self) -> bool {
        matches!(
            self,
            Self::Vorbis
                | Self::MP3
                | Self::AAC
                | Self::ADPCM
                | Self::VAG
                | Self::HEVAG
                | Self::XMA
                | Self::ATRAC9
        )
    }

    /// Check if format is lossless
    pub fn is_lossless(&self) -> bool {
        matches!(self, Self::PCM | Self::ADPCM)
    }

    /// Get typical file extension
    pub fn file_extension(&self) -> &'static str {
        match self {
            Self::PCM => "wav",
            Self::Vorbis => "ogg",
            Self::ADPCM => "wav",
            Self::MP3 => "mp3",
            Self::VAG => "vag",
            Self::HEVAG => "vag",
            Self::XMA => "xma",
            Self::AAC => "aac",
            Self::GCADPCM => "wav",
            Self::ATRAC9 => "at9",
        }
    }
}

/// Loading type for Unity AudioClip
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioLoadType {
    DecompressOnLoad = 0,
    CompressedInMemory = 1,
    Streaming = 2,
}

impl AudioLoadType {
    pub fn from_id(id: i32) -> Option<Self> {
        match id {
            0 => Some(Self::DecompressOnLoad),
            1 => Some(Self::CompressedInMemory),
            2 => Some(Self::Streaming),
            _ => None,
        }
    }
}

/// Audio metadata for different Unity versions
#[derive(Debug, Clone)]
pub enum AudioClipMeta {
    /// Legacy format (Unity < 5.0)
    Legacy {
        format: i32,
        sound_type: i32, // Simplified - not importing FMODSoundType for now
        is_3d: bool,
        use_hardware: bool,
    },
    /// Modern format (Unity >= 5.0)
    Modern {
        load_type: i32,
        channels: i32,
        frequency: i32,
        bits_per_sample: i32,
        length: f32,
        is_tracker_format: bool,
        subsound_index: i32,
        preload_audio_data: bool,
        load_in_background: bool,
        legacy_3d: bool,
        compression_format: UnityAudioFormat,
    },
}

impl Default for AudioClipMeta {
    fn default() -> Self {
        AudioClipMeta::Modern {
            load_type: 0,
            channels: 1,
            frequency: 44100,
            bits_per_sample: 16,
            length: 0.0,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data: true,
            load_in_background: false,
            legacy_3d: false,
            compression_format: UnityAudioFormat::PCM,
        }
    }
}

/// Audio load types
/// AudioClip information parsed from Unity asset (async implementation)
#[derive(Debug, Clone)]
pub struct AudioClip {
    /// Audio clip name
    pub name: String,
    /// Audio metadata (version-specific)
    pub meta: AudioClipMeta,
    /// Audio source file path (for streamed audio)
    pub source: Option<String>,
    /// Offset in source file
    pub offset: Option<i64>,
    /// Size of audio data
    pub size: i64,
    /// Raw audio data
    pub audio_data: Bytes,
    /// Streaming information (Unity 2017+)
    pub stream_info: Option<StreamingInfo>,
    /// Ambisonic audio flag (Unity 2018+)
    pub ambisonic: Option<bool>,
}

/// Streaming audio information
#[derive(Debug, Clone)]
pub struct StreamingInfo {
    pub has_resource: bool,
    pub path: String,
}

impl Default for AudioClip {
    fn default() -> Self {
        Self {
            name: String::new(),
            meta: AudioClipMeta::default(),
            source: None,
            offset: None,
            size: 0,
            audio_data: Bytes::new(),
            stream_info: None,
            ambisonic: None,
        }
    }
}

impl AudioClip {
    /// Get format from metadata
    pub fn format(&self) -> UnityAudioFormat {
        match &self.meta {
            AudioClipMeta::Modern {
                compression_format, ..
            } => *compression_format,
            AudioClipMeta::Legacy { sound_type, .. } => {
                match *sound_type {
                    14 | 28 => UnityAudioFormat::Vorbis, // OGGVORBIS | VORBIS
                    20 => UnityAudioFormat::PCM,         // WAV
                    13 => UnityAudioFormat::MP3,         // MPEG
                    _ => UnityAudioFormat::PCM,
                }
            }
        }
    }

    /// Get channel count from metadata
    pub fn channels(&self) -> u32 {
        match &self.meta {
            AudioClipMeta::Modern { channels, .. } => *channels as u32,
            AudioClipMeta::Legacy { .. } => 1, // Default assumption for legacy
        }
    }

    /// Get sample count (calculated from data size and format)
    pub fn samples(&self) -> u64 {
        match self.format() {
            UnityAudioFormat::PCM => {
                // 16-bit PCM: 2 bytes per sample per channel
                (self.audio_data.len() / (self.channels() as usize * 2)) as u64
            }
            _ => {
                // For compressed formats, approximate based on duration
                match &self.meta {
                    AudioClipMeta::Modern {
                        length, frequency, ..
                    } => (length * *frequency as f32) as u64,
                    AudioClipMeta::Legacy { .. } => 0, // Unknown for legacy without frequency
                }
            }
        }
    }

    /// Get load type from metadata
    pub fn load_type(&self) -> AudioLoadType {
        match &self.meta {
            AudioClipMeta::Modern { load_type, .. } => match *load_type {
                0 => AudioLoadType::DecompressOnLoad,
                1 => AudioLoadType::CompressedInMemory,
                2 => AudioLoadType::Streaming,
                _ => AudioLoadType::DecompressOnLoad,
            },
            AudioClipMeta::Legacy { .. } => AudioLoadType::DecompressOnLoad, // Default for legacy
        }
    }

    /// Get duration in seconds from metadata
    pub fn length(&self) -> f32 {
        match &self.meta {
            AudioClipMeta::Modern { length, .. } => *length,
            AudioClipMeta::Legacy { .. } => {
                // Calculate from samples and frequency for legacy
                0.0 // Unknown for legacy format
            }
        }
    }

    /// Create from Unity class data
    pub async fn from_unity_class(unity_class: &AsyncUnityClass) -> Result<Self> {
        let name = unity_class
            .get_property("m_Name")
            .and_then(|v| v.as_string())
            .unwrap_or("Unknown".to_string())
            .to_string();

        let format_id = unity_class
            .get_property("m_CompressionFormat")
            .and_then(|v| v.as_i32())
            .unwrap_or(1); // Default to PCM

        let format = UnityAudioFormat::from_id(format_id).unwrap_or(UnityAudioFormat::PCM);

        let load_type_id = unity_class
            .get_property("m_LoadType")
            .and_then(|v| v.as_i32())
            .unwrap_or(0);

        let load_type =
            AudioLoadType::from_id(load_type_id).unwrap_or(AudioLoadType::DecompressOnLoad);

        let sample_rate = unity_class
            .get_property("m_Frequency")
            .and_then(|v| v.as_u32())
            .unwrap_or(44100);

        let channels = unity_class
            .get_property("m_Channels")
            .and_then(|v| v.as_u32())
            .map(|c| c as u16)
            .unwrap_or(2);

        let samples = unity_class
            .get_property("m_Samples")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let audio_data = unity_class
            .get_property("m_AudioData")
            .and_then(|v| v.as_bytes())
            .cloned()
            .unwrap_or_default();

        let is_3d = unity_class
            .get_property("m_3D")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let bitrate = unity_class
            .get_property("m_BitsPerSample")
            .and_then(|v| v.as_u32());

        // Calculate length in seconds
        let length = if sample_rate > 0 {
            samples as f32 / sample_rate as f32
        } else {
            0.0
        };

        // Build AudioClipMeta based on available properties
        let meta = AudioClipMeta::Modern {
            load_type: match load_type {
                AudioLoadType::DecompressOnLoad => 0,
                AudioLoadType::CompressedInMemory => 1,
                AudioLoadType::Streaming => 2,
            },
            channels: channels as i32,
            frequency: sample_rate as i32,
            bits_per_sample: bitrate.unwrap_or(16) as i32,
            length,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data: true,
            load_in_background: false,
            legacy_3d: is_3d,
            compression_format: format,
        };

        Ok(Self {
            name,
            meta,
            source: None,
            offset: None,
            size: audio_data.len() as i64,
            audio_data: bytes::Bytes::from(audio_data),
            stream_info: None,
            ambisonic: None,
        })
    }

    /// Get audio data size in bytes
    pub fn data_size(&self) -> usize {
        self.audio_data.len()
    }

    /// Calculate expected uncompressed size
    pub fn expected_uncompressed_size(&self) -> usize {
        // Assume 16-bit samples for uncompressed size calculation
        (self.samples() * self.channels() as u64 * 2) as usize
    }

    /// Check if audio needs decompression
    pub fn needs_decompression(&self) -> bool {
        self.format().is_compressed()
            && !matches!(self.load_type(), AudioLoadType::DecompressOnLoad)
    }

    /// Get duration in milliseconds
    pub fn duration_ms(&self) -> u32 {
        (self.length() * 1000.0) as u32
    }

    /// Extract audio samples asynchronously (matching original API)
    pub async fn extract_samples(&self) -> Result<Vec<Vec<f32>>> {
        // Get format from metadata
        let format = match &self.meta {
            AudioClipMeta::Modern {
                compression_format, ..
            } => *compression_format,
            AudioClipMeta::Legacy { sound_type, .. } => {
                // Convert legacy sound type to format
                match *sound_type {
                    14 | 28 => UnityAudioFormat::Vorbis, // OGGVORBIS | VORBIS
                    20 => UnityAudioFormat::PCM,         // WAV
                    13 => UnityAudioFormat::MP3,         // MPEG
                    _ => UnityAudioFormat::PCM,
                }
            }
        };

        // Get channel count
        let channels = match &self.meta {
            AudioClipMeta::Modern { channels, .. } => *channels as usize,
            AudioClipMeta::Legacy { .. } => 1, // Default assumption for legacy
        };

        // Process audio data based on format
        match format {
            UnityAudioFormat::PCM => {
                // For PCM, convert raw bytes to samples
                let samples_per_channel = self.audio_data.len() / (channels * 2); // 16-bit samples
                let mut channels_data = vec![Vec::new(); channels];

                // Simple 16-bit PCM conversion
                for i in 0..samples_per_channel {
                    for ch in 0..channels {
                        let sample_idx = i * channels + ch;
                        if sample_idx * 2 + 1 < self.audio_data.len() {
                            let sample = i16::from_le_bytes([
                                self.audio_data[sample_idx * 2],
                                self.audio_data[sample_idx * 2 + 1],
                            ]);
                            let normalized = sample as f32 / i16::MAX as f32;
                            channels_data[ch].push(normalized);
                        }
                    }
                }
                Ok(channels_data)
            }
            _ => {
                // TODO: Implement proper compressed audio format decoding
                // Current implementation provides basic placeholders for compressed formats
                // Full implementation would require:
                // - symphonia crate for Vorbis/MP3/FLAC support
                // - Custom ADPCM decoder for Unity-specific variants
                // - Async streaming support for large audio files

                match format {
                    UnityAudioFormat::Vorbis => {
                        // TODO: Implement Vorbis decoding with symphonia crate
                        // Vorbis decoding would require ogg/vorbis decoder
                        // Return silence for now with proper channel structure
                        let sample_count = self.audio_data.len() / (channels * 2); // Estimate
                        Ok(vec![vec![0.0; sample_count.max(1024)]; channels])
                    }
                    UnityAudioFormat::MP3 => {
                        // TODO: Implement MP3 decoding with symphonia crate
                        // MP3 decoding would require mp3 decoder
                        // Return silence for now with proper channel structure
                        let sample_count = self.audio_data.len() / (channels * 2); // Estimate
                        Ok(vec![vec![0.0; sample_count.max(1024)]; channels])
                    }
                    UnityAudioFormat::ADPCM => {
                        // TODO: Improve ADPCM implementation for Unity-specific variants
                        // ADPCM has basic implementation below, but needs format-specific handling
                        AudioProcessor::process_adpcm_static(&self.audio_data, channels as u16)
                            .await
                            .map(|samples| vec![samples])
                    }
                    _ => {
                        // Unknown format, return minimal silence
                        Ok(vec![vec![0.0; 1024]; channels])
                    }
                }
            }
        }
    }

    /// Get audio information asynchronously (matching original API)
    pub async fn get_info(&self) -> AudioInfo {
        match &self.meta {
            AudioClipMeta::Modern {
                channels,
                frequency,
                compression_format,
                ..
            } => AudioInfo {
                format: *compression_format,
                properties: AudioProperties {
                    channels: *channels as u32,
                    sample_rate: *frequency as u32,
                },
            },
            AudioClipMeta::Legacy { sound_type, .. } => {
                let format = match *sound_type {
                    14 | 28 => UnityAudioFormat::Vorbis, // OGGVORBIS | VORBIS
                    20 => UnityAudioFormat::PCM,         // WAV
                    13 => UnityAudioFormat::MP3,         // MPEG
                    _ => UnityAudioFormat::PCM,
                };
                AudioInfo {
                    format,
                    properties: AudioProperties {
                        channels: 1,        // Default for legacy
                        sample_rate: 44100, // Default for legacy
                    },
                }
            }
        }
    }

    /// Get audio properties (matching original get_properties method)
    pub fn get_properties(&self) -> AudioProperties {
        match &self.meta {
            AudioClipMeta::Modern {
                channels,
                frequency,
                bits_per_sample,
                length,
                ..
            } => AudioProperties {
                channels: *channels as u32,
                sample_rate: *frequency as u32,
            },
            AudioClipMeta::Legacy { .. } => AudioProperties {
                channels: 1,
                sample_rate: 44100,
            },
        }
    }

    /// Detect audio format from magic bytes (matching original method)
    pub fn detect_format(&self) -> UnityAudioFormat {
        if self.audio_data.len() < 8 {
            return UnityAudioFormat::PCM;
        }

        let magic = &self.audio_data[..8];

        // Check for known audio format signatures
        if magic.starts_with(b"OggS") {
            UnityAudioFormat::Vorbis
        } else if magic.starts_with(b"RIFF") {
            UnityAudioFormat::PCM
        } else if magic.starts_with(&[0xFF, 0xFB]) || magic.starts_with(&[0xFF, 0xF3]) {
            UnityAudioFormat::MP3
        } else {
            // Use format from metadata
            match &self.meta {
                AudioClipMeta::Modern {
                    compression_format, ..
                } => *compression_format,
                AudioClipMeta::Legacy { sound_type, .. } => match *sound_type {
                    14 | 28 => UnityAudioFormat::Vorbis,
                    20 => UnityAudioFormat::PCM,
                    13 => UnityAudioFormat::MP3,
                    _ => UnityAudioFormat::PCM,
                },
            }
        }
    }
}

/// Audio information
#[derive(Debug)]
pub struct AudioInfo {
    pub format: UnityAudioFormat,
    pub properties: AudioProperties,
}

/// Audio properties
#[derive(Debug)]
pub struct AudioProperties {
    pub channels: u32,
    pub sample_rate: u32,
}

/// Audio processor for Unity assets
pub struct AudioProcessor {
    config: AudioConfig,
    decompressor: UnityAsyncDecompressor,
}

impl AudioProcessor {
    /// Create new audio processor
    pub fn new() -> Self {
        Self {
            config: AudioConfig::default(),
            decompressor: UnityAsyncDecompressor::new(),
        }
    }

    /// Create audio processor with configuration
    pub fn with_config(config: AudioConfig) -> Self {
        Self {
            config,
            decompressor: UnityAsyncDecompressor::new(),
        }
    }

    /// Parse AudioClip from AsyncUnityClass asynchronously (based on original from_unity_object)
    pub async fn parse_audioclip(&self, unity_object: &AsyncUnityClass) -> Result<AudioClip> {
        // Try to parse using TypeTree first (like original implementation)
        if let Some(type_tree) = unity_object.get_type_tree().await {
            let properties = unity_object.parse_with_typetree(&type_tree).await?;
            Self::from_async_typetree(&properties).await
        } else {
            // Fallback: parse from raw binary data
            let raw_data = unity_object.get_raw_data().await?;
            Self::from_async_binary_data(&raw_data).await
        }
    }

    /// Parse AudioClip from TypeTree properties asynchronously (based on original from_typetree)
    async fn from_async_typetree(properties: &HashMap<String, UnityValue>) -> Result<AudioClip> {
        let mut clip = AudioClip::default();

        // Extract name
        if let Some(name_value) = properties.get("m_Name") {
            if let Some(name) = name_value.as_string() {
                clip.name = name.to_string();
            }
        }

        // Extract audio data
        if let Some(audio_data_value) = properties.get("m_AudioData") {
            clip.audio_data = Self::extract_async_audio_data(audio_data_value).await?;
        }

        // Extract streaming info if present
        if let Some(resource_value) = properties.get("m_Resource") {
            clip.stream_info = Self::extract_async_streaming_info(resource_value).await?;
        }

        // Extract metadata - assume modern format for now (can be enhanced later)
        clip.meta = Self::extract_async_modern_meta(properties).await?;

        // Extract size
        if let Some(size_value) = properties.get("m_Size") {
            if let Some(size) = size_value.as_i64() {
                clip.size = size;
            }
        }

        // Extract ambisonic flag (Unity 2017+)
        if let Some(ambisonic_value) = properties.get("m_Ambisonic") {
            if let Some(ambisonic) = ambisonic_value.as_bool() {
                clip.ambisonic = Some(ambisonic);
            }
        }

        Ok(clip)
    }

    /// Extract audio data from UnityValue
    async fn extract_async_audio_data(audio_value: &UnityValue) -> Result<Bytes> {
        // Handle different possible formats for audio data
        match audio_value {
            UnityValue::Bytes(bytes) => Ok(Bytes::from(bytes.clone())),
            UnityValue::Array(array) => {
                // Convert array of numbers to bytes
                let mut bytes = Vec::new();
                for item in array {
                    if let Some(byte_val) = item.as_u8() {
                        bytes.push(byte_val);
                    }
                }
                Ok(Bytes::from(bytes))
            }
            _ => Ok(Bytes::new()), // Empty if can't extract
        }
    }

    /// Extract streaming info from UnityValue
    async fn extract_async_streaming_info(
        resource_value: &UnityValue,
    ) -> Result<Option<StreamingInfo>> {
        // For now, return None - can be implemented later based on Unity format
        // In original: checks for m_Source and extracts path
        Ok(None)
    }

    /// Extract modern metadata from properties (based on original extract_modern_meta)
    async fn extract_async_modern_meta(
        properties: &HashMap<String, UnityValue>,
    ) -> Result<AudioClipMeta> {
        let load_type = properties
            .get("m_LoadType")
            .and_then(|v| v.as_i32())
            .unwrap_or(0);

        let channels = properties
            .get("m_Channels")
            .and_then(|v| v.as_i32())
            .unwrap_or(1);

        let frequency = properties
            .get("m_Frequency")
            .and_then(|v| v.as_i32())
            .unwrap_or(44100);

        let bits_per_sample = properties
            .get("m_BitsPerSample")
            .and_then(|v| v.as_i32())
            .unwrap_or(16);

        let length = properties
            .get("m_Length")
            .and_then(|v| v.as_float())
            .unwrap_or(0.0);

        let preload_audio_data = properties
            .get("m_PreloadAudioData")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let load_in_background = properties
            .get("m_LoadInBackground")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let compression_format_id = properties
            .get("m_CompressionFormat")
            .and_then(|v| v.as_i32())
            .unwrap_or(0);

        let compression_format = match compression_format_id {
            0 => UnityAudioFormat::PCM,
            1 => UnityAudioFormat::Vorbis,
            2 => UnityAudioFormat::ADPCM,
            3 => UnityAudioFormat::MP3,
            _ => UnityAudioFormat::PCM,
        };

        Ok(AudioClipMeta::Modern {
            load_type,
            channels,
            frequency,
            bits_per_sample,
            length: length as f32,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data,
            load_in_background,
            legacy_3d: false,
            compression_format,
        })
    }

    /// Parse from raw binary data (fallback method)
    async fn from_async_binary_data(data: &[u8]) -> Result<AudioClip> {
        // TODO: Implement proper binary parsing for Unity AudioClip format
        // Current implementation is a basic placeholder that creates default audio data
        // Full implementation would need to:
        // - Parse Unity's binary AudioClip structure
        // - Extract format, channels, frequency from binary header
        // - Handle different Unity versions and their format variations
        // - Support embedded audio data vs external file references

        if data.len() < 16 {
            return Ok(AudioClip::default());
        }

        // Basic binary parsing attempt - this is very simplified
        let mut clip = AudioClip::default();

        // TODO: Parse actual Unity AudioClip binary header
        // Unity AudioClip binary format is complex and version-dependent
        if data.len() >= 1024 {
            // Create basic metadata for the clip
            clip.meta = AudioClipMeta::Modern {
                compression_format: UnityAudioFormat::PCM,
                load_type: 0, // DecompressOnLoad
                channels: 1,
                frequency: 44100,
                bits_per_sample: 16,
                length: 1024.0 / 44100.0, // Duration in seconds
                is_tracker_format: false,
                subsound_index: 0,
                preload_audio_data: true,
                load_in_background: false,
                legacy_3d: false,
            };
            clip.audio_data = Bytes::from(vec![0u8; 1024]);
            clip.size = 1024;
        }

        Ok(clip)
    }

    /// Process audio clip asynchronously
    pub async fn process_audio(&self, audio: &AudioClip) -> Result<ProcessedAudio> {
        // Get length from metadata
        let length = match &audio.meta {
            AudioClipMeta::Modern { length, .. } => *length,
            AudioClipMeta::Legacy { .. } => {
                // Calculate approximate length for legacy format
                if let AudioClipMeta::Legacy { .. } = &audio.meta {
                    // Use audio data size to estimate length (rough approximation)
                    audio.audio_data.len() as f32 / 44100.0 / 2.0 // Assume 16-bit mono
                } else {
                    0.0
                }
            }
        };

        // Validate audio duration
        if length > self.config.max_duration_seconds {
            return Err(UnityAssetError::parse_error(
                format!(
                    "Audio duration {:.2}s exceeds maximum {:.2}s",
                    length, self.config.max_duration_seconds
                ),
                0,
            ));
        }

        // Process audio data based on format
        let format = audio.detect_format();
        let pcm_data = match format {
            UnityAudioFormat::PCM => {
                let channels = match &audio.meta {
                    AudioClipMeta::Modern { channels, .. } => *channels as u16,
                    AudioClipMeta::Legacy { .. } => 1,
                };
                self.process_pcm(&audio.audio_data, channels).await?
            }
            UnityAudioFormat::Vorbis => {
                #[cfg(feature = "audio")]
                {
                    self.process_vorbis(&audio.audio_data).await?
                }
                #[cfg(not(feature = "audio"))]
                {
                    return Err(UnityAssetError::unsupported_format(
                        "Vorbis format requires 'audio' feature".to_string(),
                    ));
                }
            }
            UnityAudioFormat::MP3 => {
                #[cfg(feature = "audio")]
                {
                    self.process_mp3(&audio.audio_data).await?
                }
                #[cfg(not(feature = "audio"))]
                {
                    return Err(UnityAssetError::unsupported_format(
                        "MP3 format requires 'audio' feature".to_string(),
                    ));
                }
            }
            UnityAudioFormat::ADPCM => {
                self.process_adpcm(&audio.audio_data, audio.channels() as u16)
                    .await?
            }
            _ => {
                return Err(UnityAssetError::unsupported_format(format!(
                    "Audio format {:?} not yet supported",
                    audio.format()
                )));
            }
        };

        let properties = audio.get_properties();

        Ok(ProcessedAudio {
            name: audio.name.clone(),
            sample_rate: properties.sample_rate,
            channels: match &audio.meta {
                AudioClipMeta::Modern { channels, .. } => *channels as u16,
                AudioClipMeta::Legacy { .. } => 1,
            },
            samples: audio.audio_data.len() as u64 / 2, // Rough estimate for 16-bit
            original_format: format,
            pcm_data,
            length,
            is_3d: match &audio.meta {
                AudioClipMeta::Modern { legacy_3d, .. } => *legacy_3d,
                AudioClipMeta::Legacy { is_3d, .. } => *is_3d,
            },
        })
    }

    /// Process PCM format (already uncompressed)
    async fn process_pcm(&self, data: &[u8], channels: u16) -> Result<Vec<f32>> {
        if data.len() % 2 != 0 {
            return Err(UnityAssetError::parse_error(
                "PCM data length not even (16-bit samples expected)".to_string(),
                0,
            ));
        }

        let data = data.to_vec();
        task::spawn_blocking(move || {
            let mut pcm_data = Vec::with_capacity(data.len() / 2);
            for sample_bytes in data.chunks_exact(2) {
                // Convert 16-bit signed PCM to float
                let sample = i16::from_le_bytes([sample_bytes[0], sample_bytes[1]]);
                pcm_data.push(sample as f32 / i16::MAX as f32);
            }
            pcm_data
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))
    }

    /// Process Vorbis format
    #[cfg(feature = "audio")]
    async fn process_vorbis(&self, data: &[u8]) -> Result<Vec<f32>> {
        let data = data.to_vec();

        task::spawn_blocking(move || {
            let cursor = std::io::Cursor::new(data);
            let media_source = MediaSourceStream::new(Box::new(cursor), Default::default());

            let mut hint = Hint::new();
            hint.with_extension("ogg");

            let meta_opts: MetadataOptions = Default::default();
            let fmt_opts: FormatOptions = Default::default();

            let probed = symphonia::default::get_probe()
                .format(&hint, media_source, &fmt_opts, &meta_opts)
                .map_err(|e| {
                    UnityAssetError::parse_error(format!("Vorbis probe failed: {}", e), 0)
                })?;

            let mut format = probed.format;
            let track = format
                .tracks()
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
                .ok_or_else(|| {
                    UnityAssetError::parse_error("No audio tracks found".to_string(), 0)
                })?;

            let dec_opts: DecoderOptions = Default::default();
            let mut decoder = symphonia::default::get_codecs()
                .make(&track.codec_params, &dec_opts)
                .map_err(|e| {
                    UnityAssetError::parse_error(format!("Decoder creation failed: {}", e), 0)
                })?;

            let mut pcm_data = Vec::new();

            loop {
                let packet = match format.next_packet() {
                    Ok(packet) => packet,
                    Err(_) => break, // End of stream
                };

                let audio_buf = decoder
                    .decode(&packet)
                    .map_err(|e| UnityAssetError::parse_error(format!("Decode error: {}", e), 0))?;

                // Convert audio buffer to f32 samples - symphonia doesn't have as_audio_buffer()
                // Instead we need to check the buffer type and convert appropriately
                use symphonia::core::audio::{AudioBufferRef, Signal};

                match audio_buf {
                    AudioBufferRef::F32(buf) => {
                        for ch in 0..buf.spec().channels.count() {
                            let channel = buf.chan(ch);
                            for &sample in channel.iter() {
                                pcm_data.push(sample);
                            }
                        }
                    }
                    AudioBufferRef::U8(buf) => {
                        for ch in 0..buf.spec().channels.count() {
                            let channel = buf.chan(ch);
                            for &sample in channel.iter() {
                                pcm_data.push((sample as f32 - 128.0) / 128.0);
                            }
                        }
                    }
                    AudioBufferRef::U16(buf) => {
                        for ch in 0..buf.spec().channels.count() {
                            let channel = buf.chan(ch);
                            for &sample in channel.iter() {
                                pcm_data.push((sample as f32 - 32768.0) / 32768.0);
                            }
                        }
                    }
                    AudioBufferRef::S16(buf) => {
                        for ch in 0..buf.spec().channels.count() {
                            let channel = buf.chan(ch);
                            for &sample in channel.iter() {
                                pcm_data.push(sample as f32 / 32768.0);
                            }
                        }
                    }
                    _ => {
                        // For other formats, just skip
                        continue;
                    }
                }
            }

            Ok(pcm_data)
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))?
    }

    /// Process MP3 format
    #[cfg(feature = "audio")]
    async fn process_mp3(&self, data: &[u8]) -> Result<Vec<f32>> {
        let data = data.to_vec();

        task::spawn_blocking(move || {
            let cursor = std::io::Cursor::new(data);
            let media_source = MediaSourceStream::new(Box::new(cursor), Default::default());

            let mut hint = Hint::new();
            hint.with_extension("mp3");

            let meta_opts: MetadataOptions = Default::default();
            let fmt_opts: FormatOptions = Default::default();

            let probed = symphonia::default::get_probe()
                .format(&hint, media_source, &fmt_opts, &meta_opts)
                .map_err(|e| UnityAssetError::parse_error(format!("MP3 probe failed: {}", e), 0))?;

            let mut format = probed.format;
            let track = format
                .tracks()
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
                .ok_or_else(|| {
                    UnityAssetError::parse_error("No audio tracks found".to_string(), 0)
                })?;

            let dec_opts: DecoderOptions = Default::default();
            let mut decoder = symphonia::default::get_codecs()
                .make(&track.codec_params, &dec_opts)
                .map_err(|e| {
                    UnityAssetError::parse_error(format!("Decoder creation failed: {}", e), 0)
                })?;

            let mut pcm_data = Vec::new();

            loop {
                let packet = match format.next_packet() {
                    Ok(packet) => packet,
                    Err(_) => break,
                };

                let audio_buf = decoder
                    .decode(&packet)
                    .map_err(|e| UnityAssetError::parse_error(format!("Decode error: {}", e), 0))?;

                // Convert audio buffer to f32 samples - use same pattern as Vorbis
                use symphonia::core::audio::{AudioBufferRef, Signal};

                match audio_buf {
                    AudioBufferRef::F32(buf) => {
                        for ch in 0..buf.spec().channels.count() {
                            let channel = buf.chan(ch);
                            for &sample in channel.iter() {
                                pcm_data.push(sample);
                            }
                        }
                    }
                    AudioBufferRef::U8(buf) => {
                        for ch in 0..buf.spec().channels.count() {
                            let channel = buf.chan(ch);
                            for &sample in channel.iter() {
                                pcm_data.push((sample as f32 - 128.0) / 128.0);
                            }
                        }
                    }
                    AudioBufferRef::S16(buf) => {
                        for ch in 0..buf.spec().channels.count() {
                            let channel = buf.chan(ch);
                            for &sample in channel.iter() {
                                pcm_data.push(sample as f32 / 32768.0);
                            }
                        }
                    }
                    _ => {
                        // For other formats, just skip
                        continue;
                    }
                }
            }

            Ok(pcm_data)
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))?
    }

    /// Process ADPCM format (static method for use in AudioClip)
    pub async fn process_adpcm_static(data: &[u8], channels: u16) -> Result<Vec<f32>> {
        Self::process_adpcm_impl(data, channels).await
    }

    /// Process ADPCM format (basic implementation)
    async fn process_adpcm(&self, data: &[u8], channels: u16) -> Result<Vec<f32>> {
        Self::process_adpcm_impl(data, channels).await
    }

    /// Internal ADPCM processing implementation
    async fn process_adpcm_impl(data: &[u8], channels: u16) -> Result<Vec<f32>> {
        // TODO: Implement proper ADPCM decompression for Unity-specific variants
        // Current implementation is a basic linear approximation
        // Full implementation would need to:
        // - Support different ADPCM variants (IMA ADPCM, Microsoft ADPCM, etc.)
        // - Handle Unity-specific ADPCM encoding parameters
        // - Implement proper step size tables and prediction algorithms
        // - Support multi-channel ADPCM interleaving

        if data.is_empty() {
            return Ok(Vec::new());
        }

        // Basic ADPCM decompression - this is very simplified
        // Real ADPCM uses step size tables and more complex prediction
        let mut samples = Vec::new();
        let mut predictor = 0i16;
        let step_size = 256i16; // TODO: Use proper step size table

        for &byte in data {
            // Process both nibbles in each byte
            for shift in [4, 0] {
                let nibble = (byte >> shift) & 0x0F;
                let signed_nibble = if nibble & 0x08 != 0 {
                    (nibble as i16) - 16
                } else {
                    nibble as i16
                };

                // TODO: Implement proper ADPCM prediction algorithm
                // Simple prediction update (not accurate to real ADPCM)
                predictor = predictor.saturating_add(signed_nibble * step_size);

                // Convert to float sample [-1.0, 1.0]
                let sample = (predictor as f32) / 32768.0;
                samples.push(sample.clamp(-1.0, 1.0));
            }
        }

        Ok(samples)
    }

    /// Export audio to WAV format
    pub async fn export_to_wav(&self, audio: &ProcessedAudio) -> Result<Vec<u8>> {
        let sample_rate = audio.sample_rate;
        let channels = audio.channels;
        let pcm_data = audio.pcm_data.clone();

        task::spawn_blocking(move || {
            #[cfg(feature = "audio")]
            {
                use hound::{WavSpec, WavWriter};

                let spec = WavSpec {
                    channels,
                    sample_rate,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                };

                let mut output = Vec::new();
                {
                    let mut writer = WavWriter::new(std::io::Cursor::new(&mut output), spec)
                        .map_err(|e| {
                            UnityAssetError::parse_error(
                                format!("WAV writer creation failed: {}", e),
                                0,
                            )
                        })?;

                    for &sample in &pcm_data {
                        let sample_i16 = (sample * i16::MAX as f32) as i16;
                        writer.write_sample(sample_i16).map_err(|e| {
                            UnityAssetError::parse_error(format!("Sample write failed: {}", e), 0)
                        })?;
                    }

                    writer.finalize().map_err(|e| {
                        UnityAssetError::parse_error(format!("WAV finalization failed: {}", e), 0)
                    })?;
                }

                Ok(output)
            }
            #[cfg(not(feature = "audio"))]
            {
                Err(UnityAssetError::unsupported_format(
                    "WAV export requires 'audio' feature".to_string(),
                ))
            }
        })
        .await
        .map_err(|e| UnityAssetError::parse_error(format!("Task join error: {}", e), 0))?
    }

    /// Get supported formats
    pub fn supported_formats(&self) -> Vec<UnityAudioFormat> {
        let mut formats = vec![UnityAudioFormat::PCM];

        #[cfg(feature = "audio")]
        {
            formats.extend_from_slice(&[UnityAudioFormat::Vorbis, UnityAudioFormat::MP3]);
        }

        formats
    }
}

impl Default for AudioProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// Processed audio data
#[derive(Debug, Clone)]
pub struct ProcessedAudio {
    /// Audio clip name
    pub name: String,
    /// Sample rate in Hz
    pub sample_rate: u32,
    /// Number of channels
    pub channels: u16,
    /// Number of samples
    pub samples: u64,
    /// Original Unity format
    pub original_format: UnityAudioFormat,
    /// PCM audio data as f32 samples
    pub pcm_data: Vec<f32>,
    /// Length in seconds
    pub length: f32,
    /// Whether audio is 3D positioned
    pub is_3d: bool,
}

impl ProcessedAudio {
    /// Get audio data size
    pub fn data_size(&self) -> usize {
        self.pcm_data.len() * std::mem::size_of::<f32>()
    }

    /// Get duration in milliseconds
    pub fn duration_ms(&self) -> u32 {
        (self.length * 1000.0) as u32
    }

    /// Get sample at specific position
    pub fn get_sample(&self, position: usize) -> Option<f32> {
        self.pcm_data.get(position).copied()
    }

    /// Get audio peak level (for visualization)
    pub fn get_peak_level(&self) -> f32 {
        self.pcm_data.iter().map(|&s| s.abs()).fold(0.0, f32::max)
    }

    /// Get RMS level (for volume analysis)
    pub fn get_rms_level(&self) -> f32 {
        if self.pcm_data.is_empty() {
            return 0.0;
        }

        let sum_squares: f32 = self.pcm_data.iter().map(|&s| s * s).sum();
        (sum_squares / self.pcm_data.len() as f32).sqrt()
    }

    /// Convert to mono by averaging channels
    pub fn to_mono(&self) -> Vec<f32> {
        if self.channels == 1 {
            return self.pcm_data.clone();
        }

        let mut mono_data = Vec::with_capacity(self.pcm_data.len() / self.channels as usize);

        for chunk in self.pcm_data.chunks_exact(self.channels as usize) {
            let avg = chunk.iter().sum::<f32>() / chunk.len() as f32;
            mono_data.push(avg);
        }

        mono_data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unity_audio_format() {
        assert_eq!(UnityAudioFormat::from_id(1), Some(UnityAudioFormat::PCM));
        assert_eq!(UnityAudioFormat::PCM.id(), 1);
        assert!(!UnityAudioFormat::PCM.is_compressed());
        assert!(UnityAudioFormat::Vorbis.is_compressed());
        assert!(UnityAudioFormat::PCM.is_lossless());
        assert!(!UnityAudioFormat::MP3.is_lossless());
        assert_eq!(UnityAudioFormat::MP3.file_extension(), "mp3");
    }

    #[tokio::test]
    async fn test_audio_processor_creation() {
        let processor = AudioProcessor::new();
        assert_eq!(processor.config.target_sample_rate, 44100);
        assert_eq!(processor.config.target_channels, 2);
    }

    #[tokio::test]
    async fn test_pcm_processing() {
        let processor = AudioProcessor::new();
        // 16-bit stereo PCM samples: [0x7FFF, 0x0000] -> [1.0, 0.0] in float
        let input_data = vec![0xFF, 0x7F, 0x00, 0x00]; // Little-endian 16-bit samples

        let result = processor.process_pcm(&input_data, 2).await.unwrap();
        assert_eq!(result.len(), 2);
        assert!((result[0] - 1.0).abs() < 0.001); // Should be close to 1.0
        assert!((result[1] - 0.0).abs() < 0.001); // Should be close to 0.0
    }

    #[test]
    fn test_audio_load_type() {
        assert_eq!(
            AudioLoadType::from_id(0),
            Some(AudioLoadType::DecompressOnLoad)
        );
        assert_eq!(
            AudioLoadType::from_id(1),
            Some(AudioLoadType::CompressedInMemory)
        );
        assert_eq!(AudioLoadType::from_id(2), Some(AudioLoadType::Streaming));
        assert_eq!(AudioLoadType::from_id(99), None);
    }

    #[test]
    fn test_processed_audio_analysis() {
        let processed = ProcessedAudio {
            name: "test".to_string(),
            sample_rate: 44100,
            channels: 2,
            samples: 4,
            original_format: UnityAudioFormat::PCM,
            pcm_data: vec![1.0, -0.5, 0.8, -0.3], // Some test samples
            length: 1.0,
            is_3d: false,
        };

        assert_eq!(processed.duration_ms(), 1000);
        assert_eq!(processed.get_sample(0), Some(1.0));
        assert_eq!(processed.get_sample(10), None);
        assert_eq!(processed.get_peak_level(), 1.0); // Max absolute value
        assert!(processed.get_rms_level() > 0.0);

        let mono = processed.to_mono();
        assert_eq!(mono.len(), 2); // 4 samples / 2 channels = 2 mono samples
        assert_eq!(mono[0], 0.25); // (1.0 + (-0.5)) / 2
        assert_eq!(mono[1], 0.25); // (0.8 + (-0.3)) / 2
    }

    #[test]
    fn test_audio_config_defaults() {
        let config = AudioConfig::default();
        assert_eq!(config.max_duration_seconds, 300.0);
        assert_eq!(config.target_sample_rate, 44100);
        assert_eq!(config.target_channels, 2);
        assert_eq!(config.output_format, AudioOutputFormat::Wav);
    }
}
