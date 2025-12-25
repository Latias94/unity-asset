//! Texture export utilities
//!
//! This module provides functionality for exporting textures to various image formats.

use crate::error::{BinaryError, Result};
use image::{ImageFormat, RgbaImage};
use std::path::Path;

/// Texture exporter utility
///
/// This struct provides methods for exporting texture data to various image formats.
pub struct TextureExporter;

impl TextureExporter {
    /// Export texture as PNG
    ///
    /// This is the most common export format, providing lossless compression
    /// with full alpha channel support.
    pub fn export_png<P: AsRef<Path>>(image: &RgbaImage, path: P) -> Result<()> {
        image
            .save_with_format(path, ImageFormat::Png)
            .map_err(|e| BinaryError::generic(format!("Failed to save PNG: {}", e)))
    }

    /// Export texture as JPEG
    ///
    /// Note: JPEG does not support alpha channel, so alpha will be lost.
    pub fn export_jpeg<P: AsRef<Path>>(image: &RgbaImage, path: P, quality: u8) -> Result<()> {
        // Convert RGBA to RGB for JPEG (no alpha support)
        let rgb_image = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();

        let mut output = std::fs::File::create(path)
            .map_err(|e| BinaryError::generic(format!("Failed to create output file: {}", e)))?;

        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality);
        encoder
            .encode_image(&rgb_image)
            .map_err(|e| BinaryError::generic(format!("Failed to encode JPEG: {}", e)))
    }

    /// Export texture as BMP
    pub fn export_bmp<P: AsRef<Path>>(image: &RgbaImage, path: P) -> Result<()> {
        image
            .save_with_format(path, ImageFormat::Bmp)
            .map_err(|e| BinaryError::generic(format!("Failed to save BMP: {}", e)))
    }

    /// Export texture as TIFF
    pub fn export_tiff<P: AsRef<Path>>(image: &RgbaImage, path: P) -> Result<()> {
        image
            .save_with_format(path, ImageFormat::Tiff)
            .map_err(|e| BinaryError::generic(format!("Failed to save TIFF: {}", e)))
    }

    /// Export texture with automatic format detection based on file extension
    pub fn export_auto<P: AsRef<Path>>(image: &RgbaImage, path: P) -> Result<()> {
        let path_ref = path.as_ref();
        let extension = path_ref
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_lowercase();

        match extension.as_str() {
            "png" => Self::export_png(image, path),
            "jpg" | "jpeg" => Self::export_jpeg(image, path, 90), // Default quality 90
            "bmp" => Self::export_bmp(image, path),
            "tif" | "tiff" => Self::export_tiff(image, path),
            _ => {
                // Default to PNG for unknown extensions
                Self::export_png(image, path)
            }
        }
    }

    /// Export texture with custom format and options
    pub fn export_with_format<P: AsRef<Path>>(
        image: &RgbaImage,
        path: P,
        format: ImageFormat,
    ) -> Result<()> {
        image.save_with_format(path, format).map_err(|e| {
            BinaryError::generic(format!(
                "Failed to save image with format {:?}: {}",
                format, e
            ))
        })
    }

    /// Get supported export formats
    pub fn supported_formats() -> Vec<&'static str> {
        vec!["png", "jpg", "jpeg", "bmp", "tiff", "tif"]
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

    /// Validate that the image has valid dimensions for export
    pub fn validate_for_export(image: &RgbaImage) -> Result<()> {
        let (width, height) = image.dimensions();

        if width == 0 || height == 0 {
            return Err(BinaryError::invalid_data("Image has zero dimensions"));
        }

        // Check for reasonable size limits (prevent memory issues)
        if width > 32768 || height > 32768 {
            return Err(BinaryError::invalid_data(
                "Image dimensions too large for export",
            ));
        }

        Ok(())
    }

    /// Export with validation
    pub fn export_validated<P: AsRef<Path>>(image: &RgbaImage, path: P) -> Result<()> {
        Self::validate_for_export(image)?;
        Self::export_auto(image, path)
    }
}

/// Export options for advanced export scenarios
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: ImageFormat,
    pub quality: Option<u8>,     // For JPEG
    pub compression: Option<u8>, // For PNG
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ImageFormat::Png,
            quality: Some(90),
            compression: Some(6),
        }
    }
}

impl ExportOptions {
    /// Create PNG export options
    pub fn png() -> Self {
        Self {
            format: ImageFormat::Png,
            quality: None,
            compression: Some(6),
        }
    }

    /// Create JPEG export options with quality
    pub fn jpeg(quality: u8) -> Self {
        Self {
            format: ImageFormat::Jpeg,
            quality: Some(quality.clamp(1, 100)),
            compression: None,
        }
    }

    /// Create BMP export options
    pub fn bmp() -> Self {
        Self {
            format: ImageFormat::Bmp,
            quality: None,
            compression: None,
        }
    }

    /// Export with these options
    pub fn export<P: AsRef<Path>>(&self, image: &RgbaImage, path: P) -> Result<()> {
        match self.format {
            ImageFormat::Jpeg => {
                let quality = self.quality.unwrap_or(90);
                TextureExporter::export_jpeg(image, path, quality)
            }
            _ => TextureExporter::export_with_format(image, path, self.format),
        }
    }
}
