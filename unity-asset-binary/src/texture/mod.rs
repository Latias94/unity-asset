//! Texture processing module
//!
//! This module provides comprehensive texture processing capabilities for Unity assets,
//! organized following UnityPy and unity-rs best practices.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `formats` - Texture format definitions and metadata
//! - `types` - Core data structures (Texture2D, etc.)
//! - `converter` - Main conversion logic from Unity objects
//! - `decoders` - Specialized decoders for different format categories
//! - `helpers` - Utility functions for export and data manipulation
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::texture::{TextureFormat, Texture2DConverter, TextureDecoder};
//! use unity_asset_binary::unity_version::UnityVersion;
//!
//! // Create a converter
//! let converter = Texture2DConverter::new(UnityVersion::default());
//!
//! // Convert Unity object to Texture2D (assuming you have a UnityObject)
//! // let texture = converter.from_unity_object(&unity_object)?;
//!
//! // Create a decoder and decode to image
//! let decoder = TextureDecoder::new();
//! // let image = decoder.decode(&texture)?;
//!
//! // Export the image
//! // TextureExporter::export_png(&image, "output.png")?;
//! ```

pub mod formats;
pub mod types;
pub mod converter;
pub mod decoders;
pub mod helpers;

// Re-export main types for easy access
pub use formats::{TextureFormat, TextureFormatInfo};
pub use types::{Texture2D, StreamingInfo, GLTextureSettings};
pub use converter::{Texture2DConverter, Texture2DProcessor}; // Processor is legacy alias
pub use decoders::{TextureDecoder, Decoder};
pub use helpers::{TextureExporter, TextureSwizzler};

// Re-export decoder types for advanced usage
pub use decoders::{BasicDecoder, CompressedDecoder, MobileDecoder, CrunchDecoder};

// Re-export export options
pub use helpers::export::ExportOptions;

/// Main texture processing facade
/// 
/// This struct provides a high-level interface for texture processing,
/// combining conversion, decoding, and export functionality.
pub struct TextureProcessor {
    converter: Texture2DConverter,
    decoder: TextureDecoder,
}

impl TextureProcessor {
    /// Create a new texture processor
    pub fn new(version: crate::unity_version::UnityVersion) -> Self {
        Self {
            converter: Texture2DConverter::new(version),
            decoder: TextureDecoder::new(),
        }
    }

    /// Process Unity object to Texture2D
    pub fn convert_object(&self, obj: &crate::object::UnityObject) -> crate::error::Result<Texture2D> {
        self.converter.from_unity_object(obj)
    }

    /// Decode texture to RGBA image
    pub fn decode_texture(&self, texture: &Texture2D) -> crate::error::Result<image::RgbaImage> {
        self.decoder.decode(texture)
    }

    /// Full pipeline: convert object -> decode -> export
    pub fn process_and_export<P: AsRef<std::path::Path>>(
        &self,
        obj: &crate::object::UnityObject,
        output_path: P,
    ) -> crate::error::Result<()> {
        let texture = self.convert_object(obj)?;
        let image = self.decode_texture(&texture)?;
        TextureExporter::export_auto(&image, output_path)
    }

    /// Check if a format can be processed
    pub fn can_process(&self, format: TextureFormat) -> bool {
        self.decoder.can_decode(format)
    }

    /// Get list of supported formats
    pub fn supported_formats(&self) -> Vec<TextureFormat> {
        self.decoder.supported_formats()
    }
}

impl Default for TextureProcessor {
    fn default() -> Self {
        Self::new(crate::unity_version::UnityVersion::default())
    }
}

/// Convenience functions for common operations

/// Create a texture processor with default settings
pub fn create_processor() -> TextureProcessor {
    TextureProcessor::default()
}

/// Quick function to check if a format is supported
pub fn is_format_supported(format: TextureFormat) -> bool {
    let decoder = TextureDecoder::new();
    decoder.can_decode(format)
}

/// Get all supported texture formats
pub fn get_supported_formats() -> Vec<TextureFormat> {
    let decoder = TextureDecoder::new();
    decoder.supported_formats()
}

/// Quick function to decode texture data
pub fn decode_texture_data(
    format: TextureFormat,
    width: u32,
    height: u32,
    data: Vec<u8>,
) -> crate::error::Result<image::RgbaImage> {
    let texture = Texture2D {
        name: "decoded_texture".to_string(),
        width: width as i32,
        height: height as i32,
        format,
        image_data: data,
        ..Default::default()
    };
    
    let decoder = TextureDecoder::new();
    decoder.decode(&texture)
}

/// Quick function to export image with automatic format detection
pub fn export_image<P: AsRef<std::path::Path>>(
    image: &image::RgbaImage,
    path: P,
) -> crate::error::Result<()> {
    TextureExporter::export_auto(image, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_support() {
        // Basic formats should always be supported
        assert!(is_format_supported(TextureFormat::RGBA32));
        assert!(is_format_supported(TextureFormat::RGB24));
        assert!(is_format_supported(TextureFormat::ARGB32));
    }

    #[test]
    fn test_processor_creation() {
        let processor = create_processor();
        assert!(processor.can_process(TextureFormat::RGBA32));
    }

    #[test]
    fn test_supported_formats_list() {
        let formats = get_supported_formats();
        assert!(!formats.is_empty());
        assert!(formats.contains(&TextureFormat::RGBA32));
    }

    #[test]
    fn test_texture_format_info() {
        let format = TextureFormat::RGBA32;
        let info = format.info();
        assert_eq!(info.name, "RGBA32");
        assert_eq!(info.bits_per_pixel, 32);
        assert!(!info.compressed);
        assert!(info.has_alpha);
        assert!(info.supported);
    }

    #[test]
    fn test_format_categories() {
        assert!(TextureFormat::RGBA32.is_basic_format());
        assert!(!TextureFormat::RGBA32.is_compressed_format());
        assert!(!TextureFormat::RGBA32.is_mobile_format());
        assert!(!TextureFormat::RGBA32.is_crunch_compressed());

        assert!(TextureFormat::DXT1.is_compressed_format());
        assert!(!TextureFormat::DXT1.is_basic_format());

        assert!(TextureFormat::ETC2_RGB.is_mobile_format());
        assert!(!TextureFormat::ETC2_RGB.is_basic_format());

        assert!(TextureFormat::DXT1Crunched.is_crunch_compressed());
        assert!(!TextureFormat::DXT1Crunched.is_basic_format());
    }
}
