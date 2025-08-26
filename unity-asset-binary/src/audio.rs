//! AudioClip Processing and Decoding
//!
//! This module provides comprehensive AudioClip processing capabilities,
//! including format detection, decoding, and export functionality.

use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::reader::BinaryReader;
use crate::unity_version::UnityVersion;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use unity_asset_core::UnityValue;

/// Unity audio compression formats
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(i32)]
pub enum AudioCompressionFormat {
    PCM = 0,
    Vorbis = 1,
    ADPCM = 2,
    MP3 = 3,
    VAG = 4,
    HEVAG = 5,
    XMA = 6,
    AAC = 7,
    GCADPCM = 8,
    ATRAC9 = 9,
    Unknown = -1,
}

impl Default for AudioCompressionFormat {
    fn default() -> Self {
        AudioCompressionFormat::Unknown
    }
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

/// FMOD sound types (for older Unity versions)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(i32)]
pub enum FMODSoundType {
    Unknown = 0,
    ACC = 1,
    AIFF = 2,
    ASF = 3,
    AT3 = 4,
    CDDA = 5,
    DLS = 6,
    FLAC = 7,
    FSB = 8,
    GCADPCM = 9,
    IT = 10,
    MIDI = 11,
    MOD = 12,
    MPEG = 13,
    OGGVORBIS = 14,
    PLAYLIST = 15,
    RAW = 16,
    S3M = 17,
    SF2 = 18,
    USER = 19,
    WAV = 20,
    XM = 21,
    XMA = 22,
    VAG = 23,
    AUDIOQUEUE = 24,
    XWMA = 25,
    BCWAV = 26,
    AT9 = 27,
    VORBIS = 28,
    MEDIA_FOUNDATION = 29,
}

impl Default for FMODSoundType {
    fn default() -> Self {
        FMODSoundType::Unknown
    }
}

impl From<i32> for FMODSoundType {
    fn from(value: i32) -> Self {
        match value {
            1 => FMODSoundType::ACC,
            2 => FMODSoundType::AIFF,
            3 => FMODSoundType::ASF,
            4 => FMODSoundType::AT3,
            5 => FMODSoundType::CDDA,
            6 => FMODSoundType::DLS,
            7 => FMODSoundType::FLAC,
            8 => FMODSoundType::FSB,
            9 => FMODSoundType::GCADPCM,
            10 => FMODSoundType::IT,
            11 => FMODSoundType::MIDI,
            12 => FMODSoundType::MOD,
            13 => FMODSoundType::MPEG,
            14 => FMODSoundType::OGGVORBIS,
            15 => FMODSoundType::PLAYLIST,
            16 => FMODSoundType::RAW,
            17 => FMODSoundType::S3M,
            18 => FMODSoundType::SF2,
            19 => FMODSoundType::USER,
            20 => FMODSoundType::WAV,
            21 => FMODSoundType::XM,
            22 => FMODSoundType::XMA,
            23 => FMODSoundType::VAG,
            24 => FMODSoundType::AUDIOQUEUE,
            25 => FMODSoundType::XWMA,
            26 => FMODSoundType::BCWAV,
            27 => FMODSoundType::AT9,
            28 => FMODSoundType::VORBIS,
            29 => FMODSoundType::MEDIA_FOUNDATION,
            _ => FMODSoundType::Unknown,
        }
    }
}

/// Streaming info for external audio data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamingInfo {
    pub offset: u64,
    pub size: u32,
    pub path: String,
}

/// AudioClip metadata (version-dependent)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioClipMeta {
    /// Legacy format (Unity < 5.0)
    Legacy {
        format: i32,
        sound_type: FMODSoundType,
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
            channels: 1,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioClip {
    pub name: String,
    pub meta: AudioClipMeta,
    pub source: Option<String>,
    pub offset: Option<i64>,
    pub size: i64,
    pub audio_data: Vec<u8>,

    // Version-specific fields
    pub stream_info: Option<StreamingInfo>,
    pub ambisonic: Option<bool>,
}

impl Default for AudioClip {
    fn default() -> Self {
        Self {
            name: String::new(),
            meta: AudioClipMeta::default(),
            source: None,
            offset: None,
            size: 0,
            audio_data: Vec::new(),
            stream_info: None,
            ambisonic: None,
        }
    }
}

/// Audio format capabilities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFormatInfo {
    pub name: String,
    pub extension: String,
    pub compressed: bool,
    pub supported: bool,
    pub description: String,
}

impl AudioCompressionFormat {
    /// Get format information
    pub fn info(&self) -> AudioFormatInfo {
        match self {
            AudioCompressionFormat::PCM => AudioFormatInfo {
                name: "PCM".to_string(),
                extension: ".wav".to_string(),
                compressed: false,
                supported: true,
                description: "Uncompressed PCM audio".to_string(),
            },
            AudioCompressionFormat::Vorbis => AudioFormatInfo {
                name: "Vorbis".to_string(),
                extension: ".ogg".to_string(),
                compressed: true,
                supported: true,
                description: "Ogg Vorbis compressed audio".to_string(),
            },
            AudioCompressionFormat::MP3 => AudioFormatInfo {
                name: "MP3".to_string(),
                extension: ".mp3".to_string(),
                compressed: true,
                supported: true,
                description: "MP3 compressed audio".to_string(),
            },
            AudioCompressionFormat::AAC => AudioFormatInfo {
                name: "AAC".to_string(),
                extension: ".m4a".to_string(),
                compressed: true,
                supported: true,
                description: "AAC compressed audio".to_string(),
            },
            AudioCompressionFormat::ADPCM => AudioFormatInfo {
                name: "ADPCM".to_string(),
                extension: ".wav".to_string(),
                compressed: true,
                supported: false,
                description: "ADPCM compressed audio".to_string(),
            },
            _ => AudioFormatInfo {
                name: "Unknown".to_string(),
                extension: ".bin".to_string(),
                compressed: false,
                supported: false,
                description: "Unknown audio format".to_string(),
            },
        }
    }

    /// Check if format is supported for decoding
    pub fn is_supported(&self) -> bool {
        self.info().supported
    }

    /// Get file extension for this format
    pub fn extension(&self) -> &str {
        match self {
            AudioCompressionFormat::PCM => ".wav",
            AudioCompressionFormat::Vorbis => ".ogg",
            AudioCompressionFormat::MP3 => ".mp3",
            AudioCompressionFormat::AAC => ".m4a",
            _ => ".bin",
        }
    }
}

/// AudioClip processor for parsing and decoding
pub struct AudioClipProcessor {
    version: UnityVersion,
}

impl AudioClipProcessor {
    /// Create a new AudioClip processor
    pub fn new(version: UnityVersion) -> Self {
        Self { version }
    }

    /// Parse AudioClip from Unity object
    pub fn parse_audioclip(&self, object: &UnityObject) -> Result<AudioClip> {
        AudioClip::from_unity_object(object, &self.version)
    }

    /// Get supported audio formats for this Unity version
    pub fn get_supported_formats(&self) -> Vec<AudioCompressionFormat> {
        let mut formats = vec![AudioCompressionFormat::PCM, AudioCompressionFormat::Vorbis];

        // Add version-specific formats
        if self.version.major >= 5 {
            formats.extend_from_slice(&[
                AudioCompressionFormat::MP3,
                AudioCompressionFormat::AAC,
                AudioCompressionFormat::ADPCM,
            ]);
        }

        formats
    }
}

impl AudioClip {
    /// Parse AudioClip from UnityObject
    pub fn from_unity_object(obj: &UnityObject, version: &UnityVersion) -> Result<Self> {
        // Try to parse using TypeTree first
        if let Some(type_tree) = &obj.info.type_tree {
            let properties = obj.parse_with_typetree(type_tree)?;
            Self::from_typetree(&properties, version)
        } else {
            // Fallback: parse from raw binary data
            Self::from_binary_data(&obj.info.data, version)
        }
    }

    /// Parse AudioClip from TypeTree properties
    pub fn from_typetree(
        properties: &IndexMap<String, UnityValue>,
        version: &UnityVersion,
    ) -> Result<Self> {
        let mut clip = AudioClip::default();

        // Extract name
        if let Some(UnityValue::String(name)) = properties.get("m_Name") {
            clip.name = name.clone();
        }

        // Extract audio data
        if let Some(audio_data_value) = properties.get("m_AudioData") {
            clip.audio_data = Self::extract_audio_data(audio_data_value)?;
        }

        // Extract streaming info if present
        if let Some(resource_value) = properties.get("m_Resource") {
            clip.stream_info = Self::extract_streaming_info(resource_value)?;
        }

        // Extract metadata based on Unity version
        if version.major >= 5 {
            clip.meta = Self::extract_modern_meta(properties)?;
        } else {
            clip.meta = Self::extract_legacy_meta(properties)?;
        }

        // Extract size
        if let Some(UnityValue::Integer(size)) = properties.get("m_Size") {
            clip.size = *size;
        }

        // Extract ambisonic flag (Unity 2017+)
        if version.major >= 2017 {
            if let Some(UnityValue::Bool(ambisonic)) = properties.get("m_Ambisonic") {
                clip.ambisonic = Some(*ambisonic);
            }
        }

        Ok(clip)
    }

    /// Parse AudioClip from raw binary data (fallback method)
    pub fn from_binary_data(data: &[u8], version: &UnityVersion) -> Result<Self> {
        let mut reader = BinaryReader::new(data, crate::reader::ByteOrder::Little);
        let mut clip = AudioClip::default();

        // Read name (aligned string)
        clip.name = reader.read_aligned_string()?;

        // Read metadata based on version
        if version.major >= 5 {
            clip.meta = Self::read_modern_meta(&mut reader)?;
        } else {
            clip.meta = Self::read_legacy_meta(&mut reader)?;
        }

        // Read size
        clip.size = reader.read_i64()?;

        // Read audio data
        let data_size = reader.read_i32()? as usize;
        if data_size > 0 {
            clip.audio_data = reader.read_bytes(data_size)?;
        }

        Ok(clip)
    }

    /// Extract audio data from UnityValue
    fn extract_audio_data(value: &UnityValue) -> Result<Vec<u8>> {
        match value {
            UnityValue::Array(arr) => {
                let mut data = Vec::new();
                for item in arr {
                    if let UnityValue::Integer(byte_val) = item {
                        data.push(*byte_val as u8);
                    }
                }
                Ok(data)
            }
            UnityValue::String(base64_data) => {
                // Sometimes audio data is stored as base64
                use base64::{engine::general_purpose, Engine as _};
                general_purpose::STANDARD.decode(base64_data).map_err(|e| {
                    BinaryError::invalid_data(format!("Invalid base64 audio data: {}", e))
                })
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Extract streaming info from UnityValue
    fn extract_streaming_info(_value: &UnityValue) -> Result<Option<StreamingInfo>> {
        // StreamingInfo is typically a complex object with offset, size, and path
        // This is a simplified implementation
        Ok(None) // TODO: Implement full streaming info extraction
    }

    /// Extract modern metadata (Unity 5.0+)
    fn extract_modern_meta(properties: &IndexMap<String, UnityValue>) -> Result<AudioClipMeta> {
        let load_type = properties
            .get("m_LoadType")
            .and_then(|v| {
                if let UnityValue::Integer(i) = v {
                    Some(*i as i32)
                } else {
                    None
                }
            })
            .unwrap_or(0);

        let channels = properties
            .get("m_Channels")
            .and_then(|v| {
                if let UnityValue::Integer(i) = v {
                    Some(*i as i32)
                } else {
                    None
                }
            })
            .unwrap_or(1);

        let frequency = properties
            .get("m_Frequency")
            .and_then(|v| {
                if let UnityValue::Integer(i) = v {
                    Some(*i as i32)
                } else {
                    None
                }
            })
            .unwrap_or(44100);

        let bits_per_sample = properties
            .get("m_BitsPerSample")
            .and_then(|v| {
                if let UnityValue::Integer(i) = v {
                    Some(*i as i32)
                } else {
                    None
                }
            })
            .unwrap_or(16);

        let length = properties
            .get("m_Length")
            .and_then(|v| {
                if let UnityValue::Float(f) = v {
                    Some(*f as f32)
                } else {
                    None
                }
            })
            .unwrap_or(0.0);

        let compression_format = properties
            .get("m_CompressionFormat")
            .and_then(|v| {
                if let UnityValue::Integer(i) = v {
                    Some(AudioCompressionFormat::from(*i as i32))
                } else {
                    None
                }
            })
            .unwrap_or(AudioCompressionFormat::PCM);

        let preload_audio_data = properties
            .get("m_PreloadAudioData")
            .and_then(|v| {
                if let UnityValue::Bool(b) = v {
                    Some(*b)
                } else {
                    None
                }
            })
            .unwrap_or(true);

        let load_in_background = properties
            .get("m_LoadInBackground")
            .and_then(|v| {
                if let UnityValue::Bool(b) = v {
                    Some(*b)
                } else {
                    None
                }
            })
            .unwrap_or(false);

        Ok(AudioClipMeta::Modern {
            load_type,
            channels,
            frequency,
            bits_per_sample,
            length,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data,
            load_in_background,
            legacy_3d: false,
            compression_format,
        })
    }

    /// Extract legacy metadata (Unity < 5.0)
    fn extract_legacy_meta(properties: &IndexMap<String, UnityValue>) -> Result<AudioClipMeta> {
        let format = properties
            .get("m_Format")
            .and_then(|v| {
                if let UnityValue::Integer(i) = v {
                    Some(*i as i32)
                } else {
                    None
                }
            })
            .unwrap_or(0);

        let sound_type = properties
            .get("m_Type")
            .and_then(|v| {
                if let UnityValue::Integer(i) = v {
                    Some(FMODSoundType::from(*i as i32))
                } else {
                    None
                }
            })
            .unwrap_or(FMODSoundType::Unknown);

        let is_3d = properties
            .get("m_3D")
            .and_then(|v| {
                if let UnityValue::Bool(b) = v {
                    Some(*b)
                } else {
                    None
                }
            })
            .unwrap_or(false);

        let use_hardware = properties
            .get("m_UseHardware")
            .and_then(|v| {
                if let UnityValue::Bool(b) = v {
                    Some(*b)
                } else {
                    None
                }
            })
            .unwrap_or(false);

        Ok(AudioClipMeta::Legacy {
            format,
            sound_type,
            is_3d,
            use_hardware,
        })
    }

    /// Read modern metadata from binary reader
    fn read_modern_meta(reader: &mut BinaryReader) -> Result<AudioClipMeta> {
        let load_type = reader.read_i32()?;
        let channels = reader.read_i32()?;
        let frequency = reader.read_i32()?;
        let bits_per_sample = reader.read_i32()?;
        let length = reader.read_f32()?;
        let is_tracker_format = reader.read_bool()?;
        let subsound_index = reader.read_i32()?;
        let preload_audio_data = reader.read_bool()?;
        let load_in_background = reader.read_bool()?;
        let legacy_3d = reader.read_bool()?;
        let compression_format = AudioCompressionFormat::from(reader.read_i32()?);

        Ok(AudioClipMeta::Modern {
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
        })
    }

    /// Read legacy metadata from binary reader
    fn read_legacy_meta(reader: &mut BinaryReader) -> Result<AudioClipMeta> {
        let format = reader.read_i32()?;
        let sound_type = FMODSoundType::from(reader.read_i32()?);
        let is_3d = reader.read_bool()?;
        let use_hardware = reader.read_bool()?;

        Ok(AudioClipMeta::Legacy {
            format,
            sound_type,
            is_3d,
            use_hardware,
        })
    }

    /// Detect audio format from magic bytes
    pub fn detect_format(&self) -> AudioCompressionFormat {
        if self.audio_data.len() < 8 {
            return AudioCompressionFormat::Unknown;
        }

        let magic = &self.audio_data[..8];

        // Check for known audio format signatures
        if magic.starts_with(b"OggS") {
            AudioCompressionFormat::Vorbis
        } else if magic.starts_with(b"RIFF") {
            AudioCompressionFormat::PCM
        } else if magic.len() >= 8 && magic[4..8] == *b"ftyp" {
            AudioCompressionFormat::AAC
        } else if magic.starts_with(b"ID3")
            || (magic.len() >= 2 && magic[0] == 0xFF && (magic[1] & 0xE0) == 0xE0)
        {
            AudioCompressionFormat::MP3
        } else {
            // Try to get format from metadata
            match &self.meta {
                AudioClipMeta::Modern {
                    compression_format, ..
                } => *compression_format,
                AudioClipMeta::Legacy { sound_type, .. } => match sound_type {
                    FMODSoundType::OGGVORBIS | FMODSoundType::VORBIS => {
                        AudioCompressionFormat::Vorbis
                    }
                    FMODSoundType::WAV => AudioCompressionFormat::PCM,
                    FMODSoundType::MPEG => AudioCompressionFormat::MP3,
                    FMODSoundType::ACC => AudioCompressionFormat::AAC,
                    _ => AudioCompressionFormat::Unknown,
                },
            }
        }
    }

    /// Get audio properties
    pub fn get_properties(&self) -> AudioProperties {
        match &self.meta {
            AudioClipMeta::Modern {
                channels,
                frequency,
                bits_per_sample,
                length,
                compression_format,
                ..
            } => AudioProperties {
                channels: *channels,
                sample_rate: *frequency,
                bits_per_sample: *bits_per_sample,
                duration: *length,
                format: *compression_format,
                data_size: self.audio_data.len(),
            },
            AudioClipMeta::Legacy { sound_type, .. } => {
                // Legacy format has limited information
                AudioProperties {
                    channels: 1,         // Default assumption
                    sample_rate: 44100,  // Default assumption
                    bits_per_sample: 16, // Default assumption
                    duration: 0.0,       // Unknown
                    format: match sound_type {
                        FMODSoundType::OGGVORBIS | FMODSoundType::VORBIS => {
                            AudioCompressionFormat::Vorbis
                        }
                        FMODSoundType::WAV => AudioCompressionFormat::PCM,
                        FMODSoundType::MPEG => AudioCompressionFormat::MP3,
                        FMODSoundType::ACC => AudioCompressionFormat::AAC,
                        _ => AudioCompressionFormat::Unknown,
                    },
                    data_size: self.audio_data.len(),
                }
            }
        }
    }

    /// Extract audio samples to files
    pub fn extract_samples(&self) -> Result<HashMap<String, Vec<u8>>> {
        let mut samples = HashMap::new();
        let format = self.detect_format();

        match format {
            AudioCompressionFormat::Vorbis => {
                // Ogg Vorbis - can be saved directly
                let filename = format!("{}.ogg", self.name);
                samples.insert(filename, self.audio_data.clone());
            }
            AudioCompressionFormat::PCM => {
                // WAV - can be saved directly if it's already in WAV format
                if self.audio_data.starts_with(b"RIFF") {
                    let filename = format!("{}.wav", self.name);
                    samples.insert(filename, self.audio_data.clone());
                } else {
                    // Raw PCM data - need to create WAV header
                    let wav_data = self.create_wav_file()?;
                    let filename = format!("{}.wav", self.name);
                    samples.insert(filename, wav_data);
                }
            }
            AudioCompressionFormat::MP3 => {
                // MP3 - can be saved directly
                let filename = format!("{}.mp3", self.name);
                samples.insert(filename, self.audio_data.clone());
            }
            AudioCompressionFormat::AAC => {
                // AAC/M4A - can be saved directly
                let filename = format!("{}.m4a", self.name);
                samples.insert(filename, self.audio_data.clone());
            }
            _ => {
                // Unknown format - save as binary
                let filename = format!("{}.bin", self.name);
                samples.insert(filename, self.audio_data.clone());
            }
        }

        Ok(samples)
    }

    /// Create WAV file from raw PCM data
    fn create_wav_file(&self) -> Result<Vec<u8>> {
        let properties = self.get_properties();

        if properties.channels <= 0 || properties.sample_rate <= 0 {
            return Err(BinaryError::invalid_data(
                "Invalid audio properties for WAV creation",
            ));
        }

        let mut wav_data = Vec::new();

        // WAV header
        wav_data.extend_from_slice(b"RIFF");

        // File size (will be updated later)
        let file_size_pos = wav_data.len();
        wav_data.extend_from_slice(&[0u8; 4]);

        wav_data.extend_from_slice(b"WAVE");

        // Format chunk
        wav_data.extend_from_slice(b"fmt ");
        wav_data.extend_from_slice(&16u32.to_le_bytes()); // Chunk size
        wav_data.extend_from_slice(&1u16.to_le_bytes()); // Audio format (PCM)
        wav_data.extend_from_slice(&(properties.channels as u16).to_le_bytes());
        wav_data.extend_from_slice(&(properties.sample_rate as u32).to_le_bytes());

        let byte_rate =
            properties.sample_rate * properties.channels * (properties.bits_per_sample / 8);
        wav_data.extend_from_slice(&(byte_rate as u32).to_le_bytes());

        let block_align = properties.channels * (properties.bits_per_sample / 8);
        wav_data.extend_from_slice(&(block_align as u16).to_le_bytes());
        wav_data.extend_from_slice(&(properties.bits_per_sample as u16).to_le_bytes());

        // Data chunk
        wav_data.extend_from_slice(b"data");
        wav_data.extend_from_slice(&(self.audio_data.len() as u32).to_le_bytes());
        wav_data.extend_from_slice(&self.audio_data);

        // Update file size
        let total_size = wav_data.len() - 8;
        wav_data[file_size_pos..file_size_pos + 4]
            .copy_from_slice(&(total_size as u32).to_le_bytes());

        Ok(wav_data)
    }

    /// Export audio to file
    pub fn export_audio(&self, path: &str) -> Result<()> {
        let samples = self.extract_samples()?;

        if samples.is_empty() {
            return Err(BinaryError::invalid_data("No audio samples to export"));
        }

        // Use the first (and usually only) sample
        let (_, data) = samples.iter().next().unwrap();

        std::fs::write(path, data)
            .map_err(|e| BinaryError::generic(format!("Failed to write audio file: {}", e)))?;

        Ok(())
    }

    /// Get audio information summary
    pub fn get_info(&self) -> AudioInfo {
        let properties = self.get_properties();
        let format = self.detect_format();
        let format_info = format.info();

        AudioInfo {
            name: self.name.clone(),
            format,
            format_info,
            properties,
            data_size: self.audio_data.len(),
            has_external_data: self.source.is_some(),
        }
    }
}

/// Audio properties structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioProperties {
    pub channels: i32,
    pub sample_rate: i32,
    pub bits_per_sample: i32,
    pub duration: f32,
    pub format: AudioCompressionFormat,
    pub data_size: usize,
}

/// Audio information summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioInfo {
    pub name: String,
    pub format: AudioCompressionFormat,
    pub format_info: AudioFormatInfo,
    pub properties: AudioProperties,
    pub data_size: usize,
    pub has_external_data: bool,
}

/// Advanced audio decoding functionality (requires audio-support feature)
#[cfg(feature = "audio")]
impl AudioClip {
    /// Decode audio using Symphonia (supports many formats)
    pub fn decode_with_symphonia(&self) -> Result<DecodedAudio> {
        use std::io::Cursor;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::probe::Hint;

        let audio_data = self.audio_data.clone(); // Clone to avoid lifetime issues
        let cursor = Cursor::new(audio_data);
        let media_source = MediaSourceStream::new(Box::new(cursor), Default::default());

        let mut hint = Hint::new();
        match self.detect_format() {
            AudioCompressionFormat::Vorbis => hint.with_extension("ogg"),
            AudioCompressionFormat::MP3 => hint.with_extension("mp3"),
            AudioCompressionFormat::AAC => hint.with_extension("m4a"),
            _ => &mut hint,
        };

        let meta_opts = Default::default();
        let fmt_opts = Default::default();

        let probed = symphonia::default::get_probe()
            .format(&hint, media_source, &fmt_opts, &meta_opts)
            .map_err(|e| BinaryError::generic(format!("Failed to probe audio format: {}", e)))?;

        let mut format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or_else(|| BinaryError::invalid_data("No supported audio track found"))?;

        let track_id = track.id;
        let codec_params = track.codec_params.clone();

        let dec_opts = Default::default();
        let mut decoder = symphonia::default::get_codecs()
            .make(&codec_params, &dec_opts)
            .map_err(|e| BinaryError::generic(format!("Failed to create decoder: {}", e)))?;

        let mut samples = Vec::new();
        let mut sample_rate = 44100;
        let mut channels = 1;

        // Decode all packets
        loop {
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(symphonia::core::errors::Error::ResetRequired) => {
                    // The track list has been changed. Re-examine it and create a new set of decoders,
                    // then restart the decode loop. This is an advanced feature and we'll just break here.
                    break;
                }
                Err(symphonia::core::errors::Error::IoError(err)) => {
                    if err.kind() == std::io::ErrorKind::UnexpectedEof {
                        break;
                    }
                    return Err(BinaryError::generic(format!("IO error: {}", err)));
                }
                Err(err) => {
                    return Err(BinaryError::generic(format!("Decode error: {}", err)));
                }
            };

            if packet.track_id() != track_id {
                continue;
            }

            match decoder.decode(&packet) {
                Ok(decoded) => {
                    sample_rate = decoded.spec().rate;
                    channels = decoded.spec().channels.count() as u32;

                    // Convert samples to f32
                    let mut audio_buf = symphonia::core::audio::AudioBuffer::<f32>::new(
                        decoded.capacity() as u64,
                        *decoded.spec(),
                    );
                    decoded.convert(&mut audio_buf);

                    // Extract samples
                    for plane in audio_buf.planes().planes() {
                        samples.extend_from_slice(plane);
                    }
                }
                Err(symphonia::core::errors::Error::IoError(_)) => {
                    // The packet failed to decode due to an IO error, skip the packet.
                    continue;
                }
                Err(symphonia::core::errors::Error::DecodeError(_)) => {
                    // The packet failed to decode due to invalid data, skip the packet.
                    continue;
                }
                Err(err) => {
                    return Err(BinaryError::generic(format!("Decode error: {}", err)));
                }
            }
        }

        let total_samples = samples.len();
        Ok(DecodedAudio {
            samples,
            sample_rate,
            channels,
            duration: total_samples as f32 / (sample_rate * channels) as f32,
        })
    }

    /// Export decoded audio as WAV using hound
    pub fn export_decoded_wav(&self, path: &str) -> Result<()> {
        let decoded = self.decode_with_symphonia()?;

        let spec = hound::WavSpec {
            channels: decoded.channels as u16,
            sample_rate: decoded.sample_rate,
            bits_per_sample: 32, // f32 samples
            sample_format: hound::SampleFormat::Float,
        };

        let mut writer = hound::WavWriter::create(path, spec)
            .map_err(|e| BinaryError::generic(format!("Failed to create WAV writer: {}", e)))?;

        for sample in &decoded.samples {
            writer
                .write_sample(*sample)
                .map_err(|e| BinaryError::generic(format!("Failed to write sample: {}", e)))?;
        }

        writer
            .finalize()
            .map_err(|e| BinaryError::generic(format!("Failed to finalize WAV: {}", e)))?;

        Ok(())
    }

    /// Get detailed audio analysis
    pub fn analyze_audio(&self) -> Result<AudioAnalysis> {
        let decoded = self.decode_with_symphonia()?;

        // Calculate RMS (Root Mean Square) for volume analysis
        let rms = if !decoded.samples.is_empty() {
            let sum_squares: f32 = decoded.samples.iter().map(|&s| s * s).sum();
            (sum_squares / decoded.samples.len() as f32).sqrt()
        } else {
            0.0
        };

        // Find peak amplitude
        let peak = decoded
            .samples
            .iter()
            .map(|&s| s.abs())
            .fold(0.0f32, |a, b| a.max(b));

        // Calculate dynamic range (simplified)
        let dynamic_range = if rms > 0.0 {
            20.0 * (peak / rms).log10()
        } else {
            0.0
        };

        Ok(AudioAnalysis {
            duration: decoded.duration,
            sample_rate: decoded.sample_rate,
            channels: decoded.channels,
            total_samples: decoded.samples.len(),
            rms_amplitude: rms,
            peak_amplitude: peak,
            dynamic_range_db: dynamic_range,
            estimated_bitrate: self.estimate_bitrate(),
        })
    }

    /// Estimate bitrate based on file size and duration
    fn estimate_bitrate(&self) -> u32 {
        let properties = self.get_properties();
        if properties.duration > 0.0 {
            ((self.audio_data.len() * 8) as f32 / properties.duration) as u32
        } else {
            0
        }
    }
}

/// Decoded audio data
#[cfg(feature = "audio")]
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration: f32,
}

/// Audio analysis results
#[cfg(feature = "audio")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioAnalysis {
    pub duration: f32,
    pub sample_rate: u32,
    pub channels: u32,
    pub total_samples: usize,
    pub rms_amplitude: f32,
    pub peak_amplitude: f32,
    pub dynamic_range_db: f32,
    pub estimated_bitrate: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_compression_format_conversion() {
        assert_eq!(AudioCompressionFormat::from(0), AudioCompressionFormat::PCM);
        assert_eq!(
            AudioCompressionFormat::from(1),
            AudioCompressionFormat::Vorbis
        );
        assert_eq!(AudioCompressionFormat::from(3), AudioCompressionFormat::MP3);
        assert_eq!(
            AudioCompressionFormat::from(-1),
            AudioCompressionFormat::Unknown
        );
    }

    #[test]
    fn test_fmod_sound_type_conversion() {
        assert_eq!(FMODSoundType::from(20), FMODSoundType::WAV);
        assert_eq!(FMODSoundType::from(14), FMODSoundType::OGGVORBIS);
        assert_eq!(FMODSoundType::from(13), FMODSoundType::MPEG);
        assert_eq!(FMODSoundType::from(999), FMODSoundType::Unknown);
    }

    #[test]
    fn test_audio_format_info() {
        let pcm_info = AudioCompressionFormat::PCM.info();
        assert_eq!(pcm_info.name, "PCM");
        assert_eq!(pcm_info.extension, ".wav");
        assert!(!pcm_info.compressed);
        assert!(pcm_info.supported);

        let vorbis_info = AudioCompressionFormat::Vorbis.info();
        assert_eq!(vorbis_info.name, "Vorbis");
        assert_eq!(vorbis_info.extension, ".ogg");
        assert!(vorbis_info.compressed);
        assert!(vorbis_info.supported);
    }

    #[test]
    fn test_audio_format_extensions() {
        assert_eq!(AudioCompressionFormat::PCM.extension(), ".wav");
        assert_eq!(AudioCompressionFormat::Vorbis.extension(), ".ogg");
        assert_eq!(AudioCompressionFormat::MP3.extension(), ".mp3");
        assert_eq!(AudioCompressionFormat::AAC.extension(), ".m4a");
        assert_eq!(AudioCompressionFormat::Unknown.extension(), ".bin");
    }

    #[test]
    fn test_audioclip_default() {
        let clip = AudioClip::default();
        assert_eq!(clip.name, "");
        assert_eq!(clip.size, 0);
        assert!(clip.audio_data.is_empty());
        assert!(clip.source.is_none());
        assert!(clip.offset.is_none());
    }

    #[test]
    fn test_audioclip_format_detection() {
        // Test Ogg Vorbis detection
        let mut clip = AudioClip::default();
        clip.audio_data = b"OggS\x00\x02\x00\x00".to_vec();
        assert_eq!(clip.detect_format(), AudioCompressionFormat::Vorbis);

        // Test WAV detection
        clip.audio_data = b"RIFF\x24\x08\x00\x00".to_vec();
        assert_eq!(clip.detect_format(), AudioCompressionFormat::PCM);

        // Test MP4/AAC detection
        clip.audio_data = b"\x00\x00\x00\x20ftyp".to_vec();
        assert_eq!(clip.detect_format(), AudioCompressionFormat::AAC);

        // Test MP3 detection (ID3 tag)
        clip.audio_data = b"ID3\x03\x00\x00\x00\x00".to_vec();
        assert_eq!(clip.detect_format(), AudioCompressionFormat::MP3);

        // Test MP3 detection (MPEG frame header)
        clip.audio_data = b"\xFF\xFB\x90\x00\x00\x00\x00\x00".to_vec();
        assert_eq!(clip.detect_format(), AudioCompressionFormat::MP3);

        // Test unknown format (should fall back to metadata)
        clip.audio_data = b"UNKNOWN\x00".to_vec();
        clip.meta = AudioClipMeta::Modern {
            load_type: 0,
            channels: 1,
            frequency: 44100,
            bits_per_sample: 16,
            length: 1.0,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data: true,
            load_in_background: false,
            legacy_3d: false,
            compression_format: AudioCompressionFormat::Unknown,
        };
        assert_eq!(clip.detect_format(), AudioCompressionFormat::Unknown);
    }

    #[test]
    fn test_audioclip_properties() {
        let mut clip = AudioClip::default();
        clip.meta = AudioClipMeta::Modern {
            load_type: 0,
            channels: 2,
            frequency: 44100,
            bits_per_sample: 16,
            length: 5.5,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data: true,
            load_in_background: false,
            legacy_3d: false,
            compression_format: AudioCompressionFormat::PCM,
        };

        let properties = clip.get_properties();
        assert_eq!(properties.channels, 2);
        assert_eq!(properties.sample_rate, 44100);
        assert_eq!(properties.bits_per_sample, 16);
        assert_eq!(properties.duration, 5.5);
        assert_eq!(properties.format, AudioCompressionFormat::PCM);
    }

    #[test]
    fn test_audioclip_extract_samples() {
        // Test Ogg Vorbis extraction
        let mut clip = AudioClip::default();
        clip.name = "TestAudio".to_string();
        clip.audio_data = b"OggS\x00\x02\x00\x00test_ogg_data".to_vec();

        let samples = clip.extract_samples().unwrap();
        assert_eq!(samples.len(), 1);
        assert!(samples.contains_key("TestAudio.ogg"));
        assert_eq!(samples["TestAudio.ogg"], clip.audio_data);

        // Test WAV extraction
        clip.audio_data = b"RIFF\x24\x08\x00\x00WAVEtest_wav_data".to_vec();
        let samples = clip.extract_samples().unwrap();
        assert_eq!(samples.len(), 1);
        assert!(samples.contains_key("TestAudio.wav"));
        assert_eq!(samples["TestAudio.wav"], clip.audio_data);
    }

    #[test]
    fn test_wav_file_creation() {
        let mut clip = AudioClip::default();
        clip.name = "TestPCM".to_string();
        clip.meta = AudioClipMeta::Modern {
            load_type: 0,
            channels: 1,
            frequency: 22050,
            bits_per_sample: 16,
            length: 1.0,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data: true,
            load_in_background: false,
            legacy_3d: false,
            compression_format: AudioCompressionFormat::PCM,
        };

        // Raw PCM data (not WAV format)
        clip.audio_data = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05];

        let wav_data = clip.create_wav_file().unwrap();

        // Check WAV header
        assert!(wav_data.starts_with(b"RIFF"));
        assert!(wav_data[8..12] == *b"WAVE");
        assert!(wav_data[12..16] == *b"fmt ");

        // Check that original PCM data is included
        assert!(wav_data.ends_with(&clip.audio_data));
    }

    #[test]
    fn test_audioclip_processor() {
        let version = UnityVersion::from_str("2020.3.12f1").unwrap();
        let processor = AudioClipProcessor::new(version);

        let formats = processor.get_supported_formats();
        assert!(formats.contains(&AudioCompressionFormat::PCM));
        assert!(formats.contains(&AudioCompressionFormat::Vorbis));
        assert!(formats.contains(&AudioCompressionFormat::MP3));
        assert!(formats.contains(&AudioCompressionFormat::AAC));
    }

    #[test]
    fn test_audioclip_info() {
        let mut clip = AudioClip::default();
        clip.name = "InfoTest".to_string();
        clip.audio_data = b"OggS\x00\x02\x00\x00test_data".to_vec();
        clip.meta = AudioClipMeta::Modern {
            load_type: 0,
            channels: 2,
            frequency: 44100,
            bits_per_sample: 16,
            length: 3.5,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data: true,
            load_in_background: false,
            legacy_3d: false,
            compression_format: AudioCompressionFormat::Vorbis,
        };

        let info = clip.get_info();
        assert_eq!(info.name, "InfoTest");
        assert_eq!(info.format, AudioCompressionFormat::Vorbis);
        assert_eq!(info.properties.channels, 2);
        assert_eq!(info.properties.sample_rate, 44100);
        assert_eq!(info.properties.duration, 3.5);
        assert_eq!(info.data_size, clip.audio_data.len());
        assert!(!info.has_external_data);
    }

    #[test]
    fn test_legacy_audioclip_meta() {
        let clip = AudioClip {
            name: "LegacyTest".to_string(),
            meta: AudioClipMeta::Legacy {
                format: 1,
                sound_type: FMODSoundType::OGGVORBIS,
                is_3d: false,
                use_hardware: false,
            },
            source: None,
            offset: None,
            size: 1024,
            audio_data: vec![0; 1024],
            stream_info: None,
            ambisonic: None,
        };

        let properties = clip.get_properties();
        assert_eq!(properties.format, AudioCompressionFormat::Vorbis);
        assert_eq!(properties.channels, 1); // Default for legacy
        assert_eq!(properties.sample_rate, 44100); // Default for legacy
    }

    #[test]
    fn test_audioclip_from_typetree() {
        use indexmap::IndexMap;
        use unity_asset_core::UnityValue;

        let mut properties = IndexMap::new();
        properties.insert(
            "m_Name".to_string(),
            UnityValue::String("TestAudio".to_string()),
        );
        properties.insert("m_LoadType".to_string(), UnityValue::Integer(0));
        properties.insert("m_Channels".to_string(), UnityValue::Integer(2));
        properties.insert("m_Frequency".to_string(), UnityValue::Integer(44100));
        properties.insert("m_BitsPerSample".to_string(), UnityValue::Integer(16));
        properties.insert("m_Length".to_string(), UnityValue::Float(3.5));
        properties.insert("m_CompressionFormat".to_string(), UnityValue::Integer(1)); // Vorbis
        properties.insert("m_PreloadAudioData".to_string(), UnityValue::Bool(true));
        properties.insert("m_LoadInBackground".to_string(), UnityValue::Bool(false));
        properties.insert("m_Size".to_string(), UnityValue::Integer(1024));

        // Mock audio data as array of bytes
        let audio_data = vec![
            UnityValue::Integer(0x4F),
            UnityValue::Integer(0x67), // "Og"
            UnityValue::Integer(0x67),
            UnityValue::Integer(0x53), // "gS"
            UnityValue::Integer(0x00),
            UnityValue::Integer(0x02),
            UnityValue::Integer(0x00),
            UnityValue::Integer(0x00),
        ];
        properties.insert("m_AudioData".to_string(), UnityValue::Array(audio_data));

        let version = UnityVersion::from_str("2020.3.12f1").unwrap();
        let clip = AudioClip::from_typetree(&properties, &version).unwrap();

        assert_eq!(clip.name, "TestAudio");
        assert_eq!(clip.size, 1024);

        if let AudioClipMeta::Modern {
            channels,
            frequency,
            compression_format,
            ..
        } = clip.meta
        {
            assert_eq!(channels, 2);
            assert_eq!(frequency, 44100);
            assert_eq!(compression_format, AudioCompressionFormat::Vorbis);
        } else {
            panic!("Expected modern metadata");
        }

        // Verify audio data was extracted
        assert_eq!(clip.audio_data.len(), 8);
        assert!(clip.audio_data.starts_with(b"OggS"));
    }

    #[test]
    fn test_audioclip_from_binary_data() {
        // Create mock binary data for a modern AudioClip
        let mut data = Vec::new();

        // Name (aligned string) - "TestBinary"
        let name_bytes = b"TestBinary";
        data.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        data.extend_from_slice(name_bytes);
        // Align to 4 bytes
        while data.len() % 4 != 0 {
            data.push(0);
        }

        // Modern metadata
        data.extend_from_slice(&0i32.to_le_bytes()); // load_type
        data.extend_from_slice(&2i32.to_le_bytes()); // channels
        data.extend_from_slice(&44100i32.to_le_bytes()); // frequency
        data.extend_from_slice(&16i32.to_le_bytes()); // bits_per_sample
        data.extend_from_slice(&2.5f32.to_le_bytes()); // length
        data.push(0); // is_tracker_format (false)
        data.extend_from_slice(&0i32.to_le_bytes()); // subsound_index
        data.push(1); // preload_audio_data (true)
        data.push(0); // load_in_background (false)
        data.push(0); // legacy_3d (false)
        data.extend_from_slice(&1i32.to_le_bytes()); // compression_format (Vorbis)

        // Size
        data.extend_from_slice(&1024i64.to_le_bytes());

        // Audio data
        let audio_data = b"OggS\x00\x02\x00\x00test_data";
        data.extend_from_slice(&(audio_data.len() as i32).to_le_bytes());
        data.extend_from_slice(audio_data);

        let version = UnityVersion::from_str("2020.3.12f1").unwrap();
        let clip = AudioClip::from_binary_data(&data, &version).unwrap();

        assert_eq!(clip.name, "TestBinary");
        assert_eq!(clip.size, 1024);

        if let AudioClipMeta::Modern {
            channels,
            frequency,
            compression_format,
            ..
        } = clip.meta
        {
            assert_eq!(channels, 2);
            assert_eq!(frequency, 44100);
            assert_eq!(compression_format, AudioCompressionFormat::Vorbis);
        } else {
            panic!("Expected modern metadata");
        }

        assert!(clip.audio_data.starts_with(b"OggS"));
    }

    #[test]
    fn test_extract_audio_data_from_unity_value() {
        // Test array format
        let array_data = vec![
            UnityValue::Integer(0x4F),
            UnityValue::Integer(0x67),
            UnityValue::Integer(0x67),
            UnityValue::Integer(0x53),
        ];
        let array_value = UnityValue::Array(array_data);
        let extracted = AudioClip::extract_audio_data(&array_value).unwrap();
        assert_eq!(extracted, vec![0x4F, 0x67, 0x67, 0x53]);

        // Test base64 format
        use base64::{engine::general_purpose, Engine as _};
        let base64_data = general_purpose::STANDARD.encode(b"OggS\x00\x02");
        let base64_value = UnityValue::String(base64_data);
        let extracted = AudioClip::extract_audio_data(&base64_value).unwrap();
        assert_eq!(extracted, b"OggS\x00\x02");

        // Test empty/invalid format
        let empty_value = UnityValue::Null;
        let extracted = AudioClip::extract_audio_data(&empty_value).unwrap();
        assert!(extracted.is_empty());
    }
}
