//! Audio data structures
//!
//! This module defines the core data structures used for audio processing.

use super::formats::AudioCompressionFormat;
use serde::{Deserialize, Serialize};

/// Streaming info for external audio data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamingInfo {
    pub offset: u64,
    pub size: u32,
    pub path: String,
}

/// AudioClip metadata variants for different Unity versions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioClipMeta {
    /// Legacy format (Unity < 5.0)
    Legacy {
        format: i32,
        type_: i32,
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
        compression_format: AudioCompressionFormat,
    },
}

impl Default for AudioClipMeta {
    fn default() -> Self {
        AudioClipMeta::Modern {
            load_type: 0,
            channels: 2,
            frequency: 44100,
            bits_per_sample: 16,
            length: 0.0,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data: true,
            load_in_background: false,
            legacy_3d: false,
            compression_format: AudioCompressionFormat::PCM,
        }
    }
}

/// AudioClip object representation
///
/// This structure contains all the data needed to represent a Unity AudioClip object.
/// It includes both metadata and the actual audio data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioClip {
    pub name: String,
    pub meta: AudioClipMeta,
    pub source: Option<String>,
    pub offset: u64,
    pub size: u64,
    pub stream_info: StreamingInfo,
    pub data: Vec<u8>,

    // Version-specific fields
    pub ambisonic: Option<bool>,
}

impl Default for AudioClip {
    fn default() -> Self {
        Self {
            name: String::new(),
            meta: AudioClipMeta::default(),
            source: None,
            offset: 0,
            size: 0,
            stream_info: StreamingInfo::default(),
            data: Vec::new(),
            ambisonic: None,
        }
    }
}

impl AudioClip {
    /// Create a new AudioClip with basic parameters
    pub fn new(name: String, format: AudioCompressionFormat) -> Self {
        Self {
            name,
            meta: AudioClipMeta::Modern {
                compression_format: format,
                load_type: 0,
                channels: 2,
                frequency: 44100,
                bits_per_sample: 16,
                length: 0.0,
                is_tracker_format: false,
                subsound_index: 0,
                preload_audio_data: true,
                load_in_background: false,
                legacy_3d: false,
            },
            ..Default::default()
        }
    }

    /// Get compression format
    pub fn compression_format(&self) -> AudioCompressionFormat {
        match &self.meta {
            AudioClipMeta::Legacy { .. } => AudioCompressionFormat::Unknown,
            AudioClipMeta::Modern {
                compression_format, ..
            } => *compression_format,
        }
    }

    /// Get audio properties
    pub fn properties(&self) -> AudioProperties {
        match &self.meta {
            AudioClipMeta::Legacy { .. } => AudioProperties {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 16,
                length: 0.0,
            },
            AudioClipMeta::Modern {
                channels,
                frequency,
                bits_per_sample,
                length,
                ..
            } => AudioProperties {
                channels: *channels,
                sample_rate: *frequency,
                bits_per_sample: *bits_per_sample,
                length: *length,
            },
        }
    }

    /// Check if audio has data
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }

    /// Check if audio uses external streaming
    pub fn is_streamed(&self) -> bool {
        !self.stream_info.path.is_empty() && self.stream_info.size > 0
    }

    /// Get audio info
    pub fn info(&self) -> AudioInfo {
        let format = self.compression_format();
        let properties = self.properties();

        AudioInfo {
            name: self.name.clone(),
            format,
            format_info: format.info(),
            properties,
            has_data: self.has_data(),
            is_streamed: self.is_streamed(),
            data_size: self.data.len(),
        }
    }

    /// Validate audio clip data consistency
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("AudioClip name cannot be empty".to_string());
        }

        let properties = self.properties();
        if properties.channels <= 0 {
            return Err("Invalid channel count".to_string());
        }

        if properties.sample_rate <= 0 {
            return Err("Invalid sample rate".to_string());
        }

        if !self.has_data() && !self.is_streamed() {
            return Err("No audio data available and not streamed".to_string());
        }

        Ok(())
    }
}

/// Audio properties structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioProperties {
    pub channels: i32,
    pub sample_rate: i32,
    pub bits_per_sample: i32,
    pub length: f32,
}

/// Audio information structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioInfo {
    pub name: String,
    pub format: AudioCompressionFormat,
    pub format_info: super::formats::AudioFormatInfo,
    pub properties: AudioProperties,
    pub has_data: bool,
    pub is_streamed: bool,
    pub data_size: usize,
}

/// Decoded audio data
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration: f32,
}

impl DecodedAudio {
    /// Create new decoded audio
    pub fn new(samples: Vec<f32>, sample_rate: u32, channels: u32) -> Self {
        let duration = samples.len() as f32 / (sample_rate * channels) as f32;
        Self {
            samples,
            sample_rate,
            channels,
            duration,
        }
    }

    /// Get total sample count
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Get frame count (samples per channel)
    pub fn frame_count(&self) -> usize {
        self.samples.len() / self.channels as usize
    }

    /// Convert to interleaved i16 samples
    pub fn to_i16_samples(&self) -> Vec<i16> {
        self.samples
            .iter()
            .map(|&sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect()
    }

    /// Convert to interleaved i32 samples
    pub fn to_i32_samples(&self) -> Vec<i32> {
        self.samples
            .iter()
            .map(|&sample| (sample.clamp(-1.0, 1.0) * i32::MAX as f32) as i32)
            .collect()
    }
}

/// Audio analysis results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioAnalysis {
    pub duration: f32,
    pub sample_rate: u32,
    pub channels: u32,
    pub bit_depth: u32,
    pub format: AudioCompressionFormat,
    pub file_size: usize,
    pub peak_amplitude: f32,
    pub rms_amplitude: f32,
}

impl AudioAnalysis {
    /// Create analysis from decoded audio
    pub fn from_decoded(
        decoded: &DecodedAudio,
        format: AudioCompressionFormat,
        file_size: usize,
    ) -> Self {
        let peak_amplitude = decoded
            .samples
            .iter()
            .map(|&s| s.abs())
            .fold(0.0f32, f32::max);

        let rms_amplitude = if !decoded.samples.is_empty() {
            let sum_squares: f32 = decoded.samples.iter().map(|&s| s * s).sum();
            (sum_squares / decoded.samples.len() as f32).sqrt()
        } else {
            0.0
        };

        Self {
            duration: decoded.duration,
            sample_rate: decoded.sample_rate,
            channels: decoded.channels,
            bit_depth: 32, // f32 samples
            format,
            file_size,
            peak_amplitude,
            rms_amplitude,
        }
    }
}
