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
