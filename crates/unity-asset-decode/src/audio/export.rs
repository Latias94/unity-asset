//! Audio export utilities
//!
//! This module provides functionality for exporting audio to various formats.

use super::formats::AudioCompressionFormat;
use super::fsb5::{self, Fsb5Error, PreparedVorbisOgg};
use super::ogg::is_ogg_vorbis;
use super::types::{AudioClip, DecodedAudio};
use crate::error::{BinaryError, Result};
use std::io::{self, Write};
use std::path::Path;
use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError, DigestV1};

/// A validated standard audio artifact tied to the exact source bytes used at
/// preparation time.
pub struct PreparedAudioSource {
    source_length: u64,
    source_digest: DigestV1,
    encoding: PreparedAudioEncoding,
}

enum PreparedAudioEncoding {
    Passthrough,
    Fsb5Vorbis(PreparedVorbisOgg),
}

impl PreparedAudioSource {
    /// Write the prepared artifact without reparsing or allocating from
    /// attacker-controlled source metadata.
    pub fn write_to<W: Write + ?Sized>(
        &self,
        bytes: &[u8],
        writer: &mut W,
    ) -> std::result::Result<(), AudioSourceError> {
        let source_length = u64::try_from(bytes.len())
            .map_err(|_| AudioSourceError::InvalidData("audio source length exceeds u64".into()))?;
        if source_length != self.source_length || DigestV1::hash_bytes(bytes) != self.source_digest
        {
            return Err(AudioSourceError::SourceChanged);
        }
        match &self.encoding {
            PreparedAudioEncoding::Passthrough => {
                writer.write_all(bytes).map_err(AudioSourceError::Output)
            }
            PreparedAudioEncoding::Fsb5Vorbis(prepared) => prepared
                .write_to(bytes, writer)
                .map_err(AudioSourceError::from_fsb5),
        }
    }
}

/// Failures while validating or writing a playable audio artifact.
#[derive(Debug, Error)]
pub enum AudioSourceError {
    #[error("invalid audio source data: {0}")]
    InvalidData(String),
    #[error("audio compression format {0:?} has no validated standard-container export")]
    UnsupportedFormat(AudioCompressionFormat),
    #[error("audio source bytes changed after preparation")]
    SourceChanged,
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to write audio output: {0}")]
    Output(#[source] io::Error),
}

impl AudioSourceError {
    fn from_fsb5(error: Fsb5Error) -> Self {
        match error {
            Fsb5Error::Budget(error) => Self::Budget(error),
            Fsb5Error::Output(error) => Self::Output(error),
            error => Self::InvalidData(error.to_string()),
        }
    }
}

/// Audio exporter utility
///
/// This struct provides methods for exporting decoded audio data to various formats.
pub struct AudioExporter;

impl AudioExporter {
    /// Validate a Unity audio payload and prepare a deterministic standard
    /// container export under the caller's load budget.
    pub fn prepare_standard_source(
        clip: &AudioClip,
        bytes: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<PreparedAudioSource, AudioSourceError> {
        if bytes.is_empty() {
            return Err(AudioSourceError::InvalidData(
                "audio source data is empty".into(),
            ));
        }
        let source_length = u64::try_from(bytes.len())
            .map_err(|_| AudioSourceError::InvalidData("audio source length exceeds u64".into()))?;
        budget.consume_bytes(source_length)?;

        let encoding = match clip.compression_format() {
            AudioCompressionFormat::Vorbis if is_ogg_vorbis(bytes) => {
                PreparedAudioEncoding::Passthrough
            }
            AudioCompressionFormat::Vorbis if fsb5::is_fsb5(bytes) => {
                let subsound_index = usize::try_from(clip.subsound_index()).map_err(|_| {
                    AudioSourceError::InvalidData(
                        "AudioClip subsound index cannot be negative".into(),
                    )
                })?;
                PreparedAudioEncoding::Fsb5Vorbis(
                    PreparedVorbisOgg::prepare(bytes, subsound_index, budget)
                        .map_err(AudioSourceError::from_fsb5)?,
                )
            }
            AudioCompressionFormat::Vorbis => {
                return Err(AudioSourceError::InvalidData(
                    "Vorbis AudioClip data is neither an Ogg stream nor an FSB5 bank".into(),
                ));
            }
            AudioCompressionFormat::PCM | AudioCompressionFormat::ADPCM if is_wave(bytes) => {
                PreparedAudioEncoding::Passthrough
            }
            AudioCompressionFormat::MP3 if is_mp3(bytes) => PreparedAudioEncoding::Passthrough,
            AudioCompressionFormat::AAC if is_adts_aac(bytes) => PreparedAudioEncoding::Passthrough,
            format => return Err(AudioSourceError::UnsupportedFormat(format)),
        };

        Ok(PreparedAudioSource {
            source_length,
            source_digest: DigestV1::hash_bytes(bytes),
            encoding,
        })
    }

    /// Write a standard playable container from an AudioClip payload.
    ///
    /// Vorbis clips may carry an FSB5 bank instead of an Ogg stream. In that
    /// case this method reconstructs the selected subsound as Ogg/Vorbis. It
    /// rejects unknown Vorbis payloads rather than emitting FSB bytes with an
    /// `.ogg` extension.
    pub fn write_standard_source<W: Write + ?Sized>(
        clip: &AudioClip,
        bytes: &[u8],
        writer: &mut W,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<(), AudioSourceError> {
        Self::prepare_standard_source(clip, bytes, budget)?.write_to(bytes, writer)
    }

    /// Export audio as WAV file
    ///
    /// This is the most common export format, providing uncompressed audio
    /// with full quality preservation.
    pub fn export_wav<P: AsRef<Path>>(audio: &DecodedAudio, path: P) -> Result<()> {
        use std::fs::File;
        use std::io::BufWriter;

        let file = File::create(path)
            .map_err(|e| BinaryError::generic(format!("Failed to create WAV file: {}", e)))?;
        let mut writer = BufWriter::new(file);
        Self::write_wav(audio, &mut writer)?;
        writer
            .flush()
            .map_err(|e| BinaryError::generic(format!("Flush error: {}", e)))?;
        Ok(())
    }

    /// Write WAV bytes to a caller-owned sink.
    ///
    /// The sink is not flushed. Callers that buffer output retain control over
    /// the flush and durability policy.
    pub fn write_wav<W: Write + ?Sized>(audio: &DecodedAudio, writer: &mut W) -> Result<()> {
        let byte_rate = audio.sample_rate * audio.channels * 2; // 16-bit samples
        let block_align = audio.channels * 2;
        let data_size = audio.samples.len() * 2;
        let file_size = 36 + data_size;

        // Write WAV header
        writer
            .write_all(b"RIFF")
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer
            .write_all(&(file_size as u32).to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer
            .write_all(b"WAVE")
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;

        // Write format chunk
        writer
            .write_all(b"fmt ")
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer
            .write_all(&16u32.to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?; // Chunk size
        writer
            .write_all(&1u16.to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?; // Audio format (PCM)
        writer
            .write_all(&(audio.channels as u16).to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer
            .write_all(&audio.sample_rate.to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer
            .write_all(&byte_rate.to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer
            .write_all(&(block_align as u16).to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer
            .write_all(&16u16.to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?; // Bits per sample

        // Write data chunk
        writer
            .write_all(b"data")
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer
            .write_all(&(data_size as u32).to_le_bytes())
            .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;

        // Write sample data
        write_i16_samples(&audio.samples, writer)?;

        Ok(())
    }

    /// Export audio as raw PCM data
    pub fn export_raw_pcm<P: AsRef<Path>>(
        audio: &DecodedAudio,
        path: P,
        bit_depth: u8,
    ) -> Result<()> {
        use std::fs::File;
        use std::io::BufWriter;

        let file = File::create(path)
            .map_err(|e| BinaryError::generic(format!("Failed to create PCM file: {}", e)))?;
        let mut writer = BufWriter::new(file);
        Self::write_raw_pcm(audio, &mut writer, bit_depth)?;
        writer
            .flush()
            .map_err(|e| BinaryError::generic(format!("Flush error: {}", e)))?;
        Ok(())
    }

    /// Write raw little-endian PCM bytes to a caller-owned sink.
    ///
    /// Supported bit depths are 16 and 32. The sink is not flushed.
    pub fn write_raw_pcm<W: Write + ?Sized>(
        audio: &DecodedAudio,
        writer: &mut W,
        bit_depth: u8,
    ) -> Result<()> {
        match bit_depth {
            16 => {
                write_i16_samples(&audio.samples, writer)?;
            }
            32 => {
                for &sample in &audio.samples {
                    let sample = (sample.clamp(-1.0, 1.0) * i32::MAX as f32) as i32;
                    writer
                        .write_all(&sample.to_le_bytes())
                        .map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
                }
            }
            _ => {
                return Err(BinaryError::invalid_data(
                    "Unsupported bit depth for PCM export",
                ));
            }
        }

        Ok(())
    }

    /// Export audio with automatic format detection based on file extension
    pub fn export_auto<P: AsRef<Path>>(audio: &DecodedAudio, path: P) -> Result<()> {
        let path_ref = path.as_ref();
        let extension = path_ref
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_lowercase();

        match extension.as_str() {
            "wav" => Self::export_wav(audio, path),
            "pcm" | "raw" => Self::export_raw_pcm(audio, path, 16),
            _ => {
                // Default to WAV for unknown extensions
                Self::export_wav(audio, path)
            }
        }
    }

    /// Get supported export formats
    pub fn supported_formats() -> Vec<&'static str> {
        vec!["wav", "pcm", "raw"]
    }

    /// Check if a format is supported for export
    pub fn is_format_supported(extension: &str) -> bool {
        Self::supported_formats().contains(&extension.to_lowercase().as_str())
    }

    /// Create a filename with the given base name and format extension
    pub fn create_filename(base_name: &str, format: &str) -> String {
        let clean_base =
            base_name.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
        format!("{}.{}", clean_base, format.to_lowercase())
    }

    /// Validate that the audio has valid properties for export
    pub fn validate_for_export(audio: &DecodedAudio) -> Result<()> {
        if audio.samples.is_empty() {
            return Err(BinaryError::invalid_data("Audio has no samples"));
        }

        if audio.sample_rate == 0 {
            return Err(BinaryError::invalid_data("Invalid sample rate"));
        }

        if audio.channels == 0 {
            return Err(BinaryError::invalid_data("Invalid channel count"));
        }

        // Check for reasonable limits
        if audio.sample_rate > 192000 {
            return Err(BinaryError::invalid_data("Sample rate too high"));
        }

        if audio.channels > 32 {
            return Err(BinaryError::invalid_data("Too many channels"));
        }

        Ok(())
    }

    /// Export with validation
    pub fn export_validated<P: AsRef<Path>>(audio: &DecodedAudio, path: P) -> Result<()> {
        Self::validate_for_export(audio)?;
        Self::export_auto(audio, path)
    }
}

fn write_i16_samples<W: Write + ?Sized>(samples: &[f32], writer: &mut W) -> Result<()> {
    for &sample in samples {
        let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer
            .write_all(&sample.to_le_bytes())
            .map_err(|error| BinaryError::generic(format!("Write error: {error}")))?;
    }
    Ok(())
}

fn is_wave(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return false;
    }
    let declared = u32::from_le_bytes(bytes[4..8].try_into().expect("four-byte RIFF length"));
    usize::try_from(declared)
        .ok()
        .and_then(|length| length.checked_add(8))
        == Some(bytes.len())
}

fn is_mp3(bytes: &[u8]) -> bool {
    if bytes.len() >= 10 && bytes.starts_with(b"ID3") {
        return true;
    }
    bytes.get(..4).is_some_and(|header| {
        header[0] == 0xFF
            && header[1] & 0xE0 == 0xE0
            && header[1] >> 3 & 0x03 != 0x01
            && header[1] >> 1 & 0x03 != 0
            && header[2] >> 4 != 0
            && header[2] >> 4 != 0x0F
            && header[2] >> 2 & 0x03 != 0x03
    })
}

fn is_adts_aac(bytes: &[u8]) -> bool {
    bytes.get(..7).is_some_and(|header| {
        if header[0] != 0xFF || header[1] & 0xF6 != 0xF0 {
            return false;
        }
        let frame_length = (usize::from(header[3] & 0x03) << 11)
            | (usize::from(header[4]) << 3)
            | usize::from(header[5] >> 5);
        frame_length >= 7 && frame_length <= bytes.len()
    })
}

/// Export options for advanced export scenarios
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: AudioFormat,
    pub bit_depth: u8,
}

/// Supported audio export formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Wav,
    RawPcm,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: AudioFormat::Wav,
            bit_depth: 16,
        }
    }
}

impl ExportOptions {
    /// Create WAV export options
    pub fn wav() -> Self {
        Self {
            format: AudioFormat::Wav,
            bit_depth: 16,
        }
    }

    /// Create raw PCM export options with bit depth
    pub fn raw_pcm(bit_depth: u8) -> Self {
        Self {
            format: AudioFormat::RawPcm,
            bit_depth,
        }
    }

    /// Export with these options
    pub fn export<P: AsRef<Path>>(&self, audio: &DecodedAudio, path: P) -> Result<()> {
        match self.format {
            AudioFormat::Wav => AudioExporter::export_wav(audio, path),
            AudioFormat::RawPcm => AudioExporter::export_raw_pcm(audio, path, self.bit_depth),
        }
    }
}
