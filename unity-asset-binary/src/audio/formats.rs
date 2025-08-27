//! Audio format definitions
//!
//! This module defines Unity audio formats and their capabilities.
//! Inspired by UnityPy audio format handling and unity-rs simplicity.

use serde::{Deserialize, Serialize};

/// Unity audio compression formats
///
/// This enum represents all audio compression formats supported by Unity.
/// Values match Unity's internal AudioCompressionFormat enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum AudioCompressionFormat {
    /// Uncompressed PCM audio
    PCM = 0,
    /// Ogg Vorbis compression
    Vorbis = 1,
    /// ADPCM compression
    ADPCM = 2,
    /// MP3 compression
    MP3 = 3,
    /// PlayStation VAG format
    VAG = 4,
    /// PlayStation HEVAG format
    HEVAG = 5,
    /// Xbox XMA format
    XMA = 6,
    /// AAC compression
    AAC = 7,
    /// GameCube ADPCM
    GCADPCM = 8,
    /// PlayStation ATRAC9
    ATRAC9 = 9,
    /// Unknown format
    #[default]
    Unknown = -1,
}

impl From<i32> for AudioCompressionFormat {
    fn from(value: i32) -> Self {
        match value {
            0 => AudioCompressionFormat::PCM,
            1 => AudioCompressionFormat::Vorbis,
            2 => AudioCompressionFormat::ADPCM,
            3 => AudioCompressionFormat::MP3,
            4 => AudioCompressionFormat::VAG,
            5 => AudioCompressionFormat::HEVAG,
            6 => AudioCompressionFormat::XMA,
            7 => AudioCompressionFormat::AAC,
            8 => AudioCompressionFormat::GCADPCM,
            9 => AudioCompressionFormat::ATRAC9,
            _ => AudioCompressionFormat::Unknown,
        }
    }
}

/// FMOD sound type enumeration
///
/// Used for identifying audio format types in FMOD-based Unity versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[repr(i32)]
pub enum FMODSoundType {
    #[default]
    Unknown = 0,
    ACC = 1,
    AIFF = 2,
    ASF = 3,
    DLS = 4,
    FLAC = 5,
    FSB = 6,
    IT = 7,
    MIDI = 8,
    MOD = 9,
    MPEG = 10,
    OGG = 11,
    PLAYLIST = 12,
    RAW = 13,
    S3M = 14,
    USER = 15,
    WAV = 16,
    XM = 17,
    XMA = 18,
    AUDIOQUEUE = 19,
    AT9 = 20,
    VORBIS = 21,
    MediaFoundation = 22,
    MediaCodec = 23,
    FADPCM = 24,
    OPUS = 25,
}

impl From<i32> for FMODSoundType {
    fn from(value: i32) -> Self {
        match value {
            1 => FMODSoundType::ACC,
            2 => FMODSoundType::AIFF,
            3 => FMODSoundType::ASF,
            4 => FMODSoundType::DLS,
            5 => FMODSoundType::FLAC,
            6 => FMODSoundType::FSB,
            7 => FMODSoundType::IT,
            8 => FMODSoundType::MIDI,
            9 => FMODSoundType::MOD,
            10 => FMODSoundType::MPEG,
            11 => FMODSoundType::OGG,
            12 => FMODSoundType::PLAYLIST,
            13 => FMODSoundType::RAW,
            14 => FMODSoundType::S3M,
            15 => FMODSoundType::USER,
            16 => FMODSoundType::WAV,
            17 => FMODSoundType::XM,
            18 => FMODSoundType::XMA,
            19 => FMODSoundType::AUDIOQUEUE,
            20 => FMODSoundType::AT9,
            21 => FMODSoundType::VORBIS,
            22 => FMODSoundType::MediaFoundation,
            23 => FMODSoundType::MediaCodec,
            24 => FMODSoundType::FADPCM,
            25 => FMODSoundType::OPUS,
            _ => FMODSoundType::Unknown,
        }
    }
}

/// Audio format information and capabilities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFormatInfo {
    pub name: String,
    pub extension: String,
    pub compressed: bool,
    pub lossy: bool,
    pub supported: bool,
}

impl Default for AudioFormatInfo {
    fn default() -> Self {
        Self {
            name: "Unknown".to_string(),
            extension: "bin".to_string(),
            compressed: false,
            lossy: false,
            supported: false,
        }
    }
}

impl AudioCompressionFormat {
    /// Get format information
    pub fn info(&self) -> AudioFormatInfo {
        match self {
            AudioCompressionFormat::PCM => AudioFormatInfo {
                name: "PCM".to_string(),
                extension: "wav".to_string(),
                compressed: false,
                lossy: false,
                supported: true,
            },
            AudioCompressionFormat::Vorbis => AudioFormatInfo {
                name: "Ogg Vorbis".to_string(),
                extension: "ogg".to_string(),
                compressed: true,
                lossy: true,
                supported: true,
            },
            AudioCompressionFormat::ADPCM => AudioFormatInfo {
                name: "ADPCM".to_string(),
                extension: "wav".to_string(),
                compressed: true,
                lossy: true,
                supported: true,
            },
            AudioCompressionFormat::MP3 => AudioFormatInfo {
                name: "MP3".to_string(),
                extension: "mp3".to_string(),
                compressed: true,
                lossy: true,
                supported: true,
            },
            AudioCompressionFormat::AAC => AudioFormatInfo {
                name: "AAC".to_string(),
                extension: "aac".to_string(),
                compressed: true,
                lossy: true,
                supported: true,
            },
            AudioCompressionFormat::VAG => AudioFormatInfo {
                name: "PlayStation VAG".to_string(),
                extension: "vag".to_string(),
                compressed: true,
                lossy: true,
                supported: false, // Requires specialized decoder
            },
            AudioCompressionFormat::HEVAG => AudioFormatInfo {
                name: "PlayStation HEVAG".to_string(),
                extension: "vag".to_string(),
                compressed: true,
                lossy: true,
                supported: false, // Requires specialized decoder
            },
            AudioCompressionFormat::XMA => AudioFormatInfo {
                name: "Xbox XMA".to_string(),
                extension: "xma".to_string(),
                compressed: true,
                lossy: true,
                supported: false, // Requires specialized decoder
            },
            AudioCompressionFormat::GCADPCM => AudioFormatInfo {
                name: "GameCube ADPCM".to_string(),
                extension: "adp".to_string(),
                compressed: true,
                lossy: true,
                supported: false, // Requires specialized decoder
            },
            AudioCompressionFormat::ATRAC9 => AudioFormatInfo {
                name: "PlayStation ATRAC9".to_string(),
                extension: "at9".to_string(),
                compressed: true,
                lossy: true,
                supported: false, // Requires specialized decoder
            },
            AudioCompressionFormat::Unknown => AudioFormatInfo::default(),
        }
    }

    /// Check if format is supported for decoding
    pub fn is_supported(&self) -> bool {
        self.info().supported
    }

    /// Check if format is compressed
    pub fn is_compressed(&self) -> bool {
        self.info().compressed
    }

    /// Check if format is lossy
    pub fn is_lossy(&self) -> bool {
        self.info().lossy
    }

    /// Get recommended file extension
    pub fn extension(&self) -> &str {
        match self {
            AudioCompressionFormat::PCM | AudioCompressionFormat::ADPCM => "wav",
            AudioCompressionFormat::Vorbis => "ogg",
            AudioCompressionFormat::MP3 => "mp3",
            AudioCompressionFormat::AAC => "aac",
            AudioCompressionFormat::VAG | AudioCompressionFormat::HEVAG => "vag",
            AudioCompressionFormat::XMA => "xma",
            AudioCompressionFormat::GCADPCM => "adp",
            AudioCompressionFormat::ATRAC9 => "at9",
            AudioCompressionFormat::Unknown => "bin",
        }
    }

    /// Get MIME type for the format
    pub fn mime_type(&self) -> &str {
        match self {
            AudioCompressionFormat::PCM | AudioCompressionFormat::ADPCM => "audio/wav",
            AudioCompressionFormat::Vorbis => "audio/ogg",
            AudioCompressionFormat::MP3 => "audio/mpeg",
            AudioCompressionFormat::AAC => "audio/aac",
            _ => "application/octet-stream",
        }
    }
}
