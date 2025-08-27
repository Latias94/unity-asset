//! Audio export utilities
//!
//! This module provides functionality for exporting audio to various formats.

use crate::error::{BinaryError, Result};
use super::types::DecodedAudio;
use std::path::Path;

/// Audio exporter utility
/// 
/// This struct provides methods for exporting decoded audio data to various formats.
pub struct AudioExporter;

impl AudioExporter {
    /// Export audio as WAV file
    /// 
    /// This is the most common export format, providing uncompressed audio
    /// with full quality preservation.
    pub fn export_wav<P: AsRef<Path>>(audio: &DecodedAudio, path: P) -> Result<()> {
        use std::fs::File;
        use std::io::{BufWriter, Write};

        let file = File::create(path).map_err(|e| {
            BinaryError::generic(format!("Failed to create WAV file: {}", e))
        })?;
        let mut writer = BufWriter::new(file);

        // Convert f32 samples to i16
        let i16_samples = audio.to_i16_samples();
        let byte_rate = audio.sample_rate * audio.channels * 2; // 16-bit samples
        let block_align = audio.channels * 2;
        let data_size = i16_samples.len() * 2;
        let file_size = 36 + data_size;

        // Write WAV header
        writer.write_all(b"RIFF").map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer.write_all(&(file_size as u32).to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer.write_all(b"WAVE").map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;

        // Write format chunk
        writer.write_all(b"fmt ").map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer.write_all(&16u32.to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?; // Chunk size
        writer.write_all(&1u16.to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?; // Audio format (PCM)
        writer.write_all(&(audio.channels as u16).to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer.write_all(&audio.sample_rate.to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer.write_all(&byte_rate.to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer.write_all(&(block_align as u16).to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer.write_all(&16u16.to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?; // Bits per sample

        // Write data chunk
        writer.write_all(b"data").map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        writer.write_all(&(data_size as u32).to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;

        // Write sample data
        for sample in i16_samples {
            writer.write_all(&sample.to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
        }

        writer.flush().map_err(|e| BinaryError::generic(format!("Flush error: {}", e)))?;
        Ok(())
    }

    /// Export audio as raw PCM data
    pub fn export_raw_pcm<P: AsRef<Path>>(audio: &DecodedAudio, path: P, bit_depth: u8) -> Result<()> {
        use std::fs::File;
        use std::io::{BufWriter, Write};

        let file = File::create(path).map_err(|e| {
            BinaryError::generic(format!("Failed to create PCM file: {}", e))
        })?;
        let mut writer = BufWriter::new(file);

        match bit_depth {
            16 => {
                let i16_samples = audio.to_i16_samples();
                for sample in i16_samples {
                    writer.write_all(&sample.to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
                }
            }
            32 => {
                let i32_samples = audio.to_i32_samples();
                for sample in i32_samples {
                    writer.write_all(&sample.to_le_bytes()).map_err(|e| BinaryError::generic(format!("Write error: {}", e)))?;
                }
            }
            _ => {
                return Err(BinaryError::invalid_data("Unsupported bit depth for PCM export"));
            }
        }

        writer.flush().map_err(|e| BinaryError::generic(format!("Flush error: {}", e)))?;
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
        let clean_base = base_name.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
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

/// Export options for advanced export scenarios
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: AudioFormat,
    pub bit_depth: u8,
    pub sample_rate: Option<u32>, // For resampling
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
            sample_rate: None,
        }
    }
}

impl ExportOptions {
    /// Create WAV export options
    pub fn wav() -> Self {
        Self {
            format: AudioFormat::Wav,
            bit_depth: 16,
            sample_rate: None,
        }
    }

    /// Create raw PCM export options with bit depth
    pub fn raw_pcm(bit_depth: u8) -> Self {
        Self {
            format: AudioFormat::RawPcm,
            bit_depth,
            sample_rate: None,
        }
    }

    /// Set sample rate for resampling
    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = Some(sample_rate);
        self
    }

    /// Export with these options
    pub fn export<P: AsRef<Path>>(&self, audio: &DecodedAudio, path: P) -> Result<()> {
        // TODO: Implement resampling if sample_rate is specified
        let audio_to_export = audio; // For now, use original audio

        match self.format {
            AudioFormat::Wav => AudioExporter::export_wav(audio_to_export, path),
            AudioFormat::RawPcm => AudioExporter::export_raw_pcm(audio_to_export, path, self.bit_depth),
        }
    }
}
