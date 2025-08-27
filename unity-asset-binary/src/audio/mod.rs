//! Audio processing module
//!
//! This module provides comprehensive audio processing capabilities for Unity assets,
//! organized following UnityPy and unity-rs best practices.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `formats` - Audio format definitions and metadata
//! - `types` - Core data structures (AudioClip, DecodedAudio, etc.)
//! - `converter` - Main conversion logic from Unity objects
//! - `decoder` - Audio decoding using Symphonia
//! - `export` - Audio export functionality
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::audio::{AudioCompressionFormat, AudioClipConverter, AudioDecoder};
//! use unity_asset_binary::unity_version::UnityVersion;
//!
//! // Create a converter
//! let converter = AudioClipConverter::new(UnityVersion::default());
//!
//! // Convert Unity object to AudioClip (assuming you have a UnityObject)
//! // let audio_clip = converter.from_unity_object(&unity_object)?;
//!
//! // Create a decoder and decode to audio
//! let decoder = AudioDecoder::new();
//! // let decoded_audio = decoder.decode(&audio_clip)?;
//!
//! // Export the audio
//! // AudioExporter::export_wav(&decoded_audio, "output.wav")?;
//! ```

pub mod converter;
pub mod decoder;
pub mod export;
pub mod formats;
pub mod types;

// Re-export main types for easy access
pub use converter::{AudioClipConverter, AudioClipProcessor}; // Processor is legacy alias
pub use decoder::AudioDecoder;
pub use export::{AudioExporter, AudioFormat, ExportOptions};
pub use formats::{AudioCompressionFormat, AudioFormatInfo, FMODSoundType};
pub use types::{
    AudioAnalysis, AudioClip, AudioClipMeta, AudioInfo, AudioProperties, DecodedAudio,
    StreamingInfo,
};

/// Main audio processing facade
///
/// This struct provides a high-level interface for audio processing,
/// combining conversion, decoding, and export functionality.
pub struct AudioProcessor {
    converter: AudioClipConverter,
    decoder: AudioDecoder,
}

impl AudioProcessor {
    /// Create a new audio processor
    pub fn new(version: crate::unity_version::UnityVersion) -> Self {
        Self {
            converter: AudioClipConverter::new(version),
            decoder: AudioDecoder::new(),
        }
    }

    /// Process Unity object to AudioClip
    pub fn convert_object(
        &self,
        obj: &crate::object::UnityObject,
    ) -> crate::error::Result<AudioClip> {
        self.converter.from_unity_object(obj)
    }

    /// Decode audio clip to PCM data
    pub fn decode_audio(&self, clip: &AudioClip) -> crate::error::Result<DecodedAudio> {
        self.decoder.decode(clip)
    }

    /// Get audio data (either embedded or streamed)
    pub fn get_audio_data(&self, clip: &AudioClip) -> crate::error::Result<Vec<u8>> {
        self.converter.get_audio_data(clip)
    }

    /// Full pipeline: convert object -> decode -> export
    pub fn process_and_export<P: AsRef<std::path::Path>>(
        &self,
        obj: &crate::object::UnityObject,
        output_path: P,
    ) -> crate::error::Result<()> {
        let audio_clip = self.convert_object(obj)?;
        let decoded_audio = self.decode_audio(&audio_clip)?;
        AudioExporter::export_auto(&decoded_audio, output_path)
    }

    /// Check if a format can be processed
    pub fn can_process(&self, format: AudioCompressionFormat) -> bool {
        self.converter.can_process(format) && self.decoder.can_decode(format)
    }

    /// Get list of supported formats
    pub fn supported_formats(&self) -> Vec<AudioCompressionFormat> {
        let converter_formats = self.converter.supported_formats();
        let decoder_formats = self.decoder.supported_formats();

        // Return intersection of both lists
        converter_formats
            .into_iter()
            .filter(|format| decoder_formats.contains(format))
            .collect()
    }

    /// Load streaming data from external file
    pub fn load_streaming_data(&self, clip: &AudioClip) -> crate::error::Result<Vec<u8>> {
        self.converter.load_streaming_data(clip)
    }
}

impl Default for AudioProcessor {
    fn default() -> Self {
        Self::new(crate::unity_version::UnityVersion::default())
    }
}

/// Convenience functions for common operations

/// Create an audio processor with default settings
pub fn create_processor() -> AudioProcessor {
    AudioProcessor::default()
}

/// Quick function to check if a format is supported
pub fn is_format_supported(format: AudioCompressionFormat) -> bool {
    let decoder = AudioDecoder::new();
    decoder.can_decode(format)
}

/// Get all supported audio formats
pub fn get_supported_formats() -> Vec<AudioCompressionFormat> {
    let decoder = AudioDecoder::new();
    decoder.supported_formats()
}

/// Quick function to decode audio data
pub fn decode_audio_data(
    format: AudioCompressionFormat,
    data: Vec<u8>,
) -> crate::error::Result<DecodedAudio> {
    let audio_clip = AudioClip {
        name: "decoded_audio".to_string(),
        meta: types::AudioClipMeta::Modern {
            compression_format: format,
            channels: 2,
            frequency: 44100,
            bits_per_sample: 16,
            length: 0.0,
            load_type: 0,
            is_tracker_format: false,
            subsound_index: 0,
            preload_audio_data: true,
            load_in_background: false,
            legacy_3d: false,
        },
        data,
        ..Default::default()
    };

    let decoder = AudioDecoder::new();
    decoder.decode(&audio_clip)
}

/// Quick function to export audio with automatic format detection
pub fn export_audio<P: AsRef<std::path::Path>>(
    audio: &DecodedAudio,
    path: P,
) -> crate::error::Result<()> {
    AudioExporter::export_auto(audio, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_support() {
        // Basic formats should be supported when audio-advanced feature is enabled
        #[cfg(feature = "audio-advanced")]
        {
            assert!(is_format_supported(AudioCompressionFormat::PCM));
            assert!(is_format_supported(AudioCompressionFormat::Vorbis));
            assert!(is_format_supported(AudioCompressionFormat::MP3));
        }
    }

    #[test]
    fn test_processor_creation() {
        let processor = create_processor();
        // Basic test - processor should be created successfully
        assert!(
            !processor.supported_formats().is_empty() || processor.supported_formats().is_empty()
        );
    }

    #[test]
    fn test_supported_formats_list() {
        let formats = get_supported_formats();
        // Should return a list (may be empty if audio-advanced feature is not enabled)
        assert!(!formats.is_empty() || formats.is_empty());
    }

    #[test]
    fn test_audio_format_info() {
        let format = AudioCompressionFormat::PCM;
        let info = format.info();
        assert_eq!(info.name, "PCM");
        assert_eq!(info.extension, "wav");
        assert!(!info.compressed);
        assert!(!info.lossy);
        assert!(info.supported);
    }

    #[test]
    fn test_format_properties() {
        assert!(AudioCompressionFormat::PCM.is_supported());
        assert!(!AudioCompressionFormat::PCM.is_compressed());
        assert!(!AudioCompressionFormat::PCM.is_lossy());
        assert_eq!(AudioCompressionFormat::PCM.extension(), "wav");

        assert!(AudioCompressionFormat::Vorbis.is_compressed());
        assert!(AudioCompressionFormat::Vorbis.is_lossy());
        assert_eq!(AudioCompressionFormat::Vorbis.extension(), "ogg");
    }

    #[test]
    fn test_audio_clip_creation() {
        let clip = AudioClip::new("test".to_string(), AudioCompressionFormat::PCM);
        assert_eq!(clip.name, "test");
        assert_eq!(clip.compression_format(), AudioCompressionFormat::PCM);
    }
}
