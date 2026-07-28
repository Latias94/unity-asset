//! Texture artifact encoding utilities
//!
//! Every encoder writes to storage owned and managed by the caller.

use crate::error::{BinaryError, Result};
use image::{ExtendedColorType, ImageEncoder, ImageError, RgbaImage};
use std::io::{Seek, Write};

/// Texture encoder utility
///
/// This struct provides explicit image-format writers without owning filesystem paths.
pub struct TextureExporter;

pub(crate) fn write_rgba_png<W: Write + ?Sized>(
    image: &RgbaImage,
    writer: &mut W,
    error_context: &'static str,
) -> Result<()> {
    validate_dimensions(image, "PNG")?;
    image::codecs::png::PngEncoder::new(writer)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|error| map_image_error(error, error_context))
}

fn validate_dimensions(image: &RgbaImage, format: &'static str) -> Result<()> {
    if image.width() == 0 || image.height() == 0 {
        return Err(BinaryError::invalid_data(format!(
            "{format} dimensions must be non-zero"
        )));
    }
    Ok(())
}

fn map_image_error(error: ImageError, context: &'static str) -> BinaryError {
    match error {
        ImageError::IoError(source) => BinaryError::Io(source),
        error => BinaryError::generic(format!("{context}: {error}")),
    }
}

impl TextureExporter {
    /// Write PNG bytes to a caller-owned sink.
    ///
    /// The sink is not flushed. Callers that buffer output retain control over
    /// the flush and durability policy.
    pub fn write_png<W: Write + ?Sized>(image: &RgbaImage, writer: &mut W) -> Result<()> {
        write_rgba_png(image, writer, "Failed to save PNG")
    }

    /// Write JPEG bytes to a caller-owned sink.
    ///
    /// JPEG does not preserve the alpha channel. The sink is not flushed.
    pub fn write_jpeg<W: Write + ?Sized>(
        image: &RgbaImage,
        writer: &mut W,
        quality: u8,
    ) -> Result<()> {
        validate_dimensions(image, "JPEG")?;
        if image.width() > u32::from(u16::MAX) || image.height() > u32::from(u16::MAX) {
            return Err(BinaryError::invalid_data(
                "JPEG dimensions exceed the u16 wire limit",
            ));
        }
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(writer, quality);
        encoder
            .encode_image(image)
            .map_err(|error| map_image_error(error, "Failed to encode JPEG"))
    }

    /// Write BMP bytes to a caller-owned sink.
    ///
    /// The sink is not flushed.
    pub fn write_bmp<W: Write + ?Sized>(image: &RgbaImage, mut writer: &mut W) -> Result<()> {
        validate_dimensions(image, "BMP")?;
        image::codecs::bmp::BmpEncoder::new(&mut writer)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )
            .map_err(|error| map_image_error(error, "Failed to encode BMP"))
    }

    /// Write TIFF bytes to a caller-owned seekable sink.
    ///
    /// The sink is not flushed.
    pub fn write_tiff<W: Write + Seek + ?Sized>(image: &RgbaImage, writer: &mut W) -> Result<()> {
        validate_dimensions(image, "TIFF")?;
        image::codecs::tiff::TiffEncoder::new(writer)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )
            .map_err(|error| map_image_error(error, "Failed to encode TIFF"))
    }
}
