//! Texture decoder dispatch and allocation ownership.

mod basic;
mod compressed;
mod crunch;
mod mobile;

use std::collections::TryReserveError;
use std::mem::size_of;

#[cfg(feature = "texture-advanced")]
use std::fmt::Display;

use image::RgbaImage;
use unity_asset_core::{AssetLoadBudget, BudgetError};

use self::basic::BasicDecoder;
use self::compressed::CompressedDecoder;
use self::crunch::CrunchDecoder;
use self::mobile::MobileDecoder;
use super::formats::TextureFormat;
use super::types::Texture2D;
use unity_asset_binary::{BinaryError, Result};

/// Dispatches supported Unity texture encodings into one RGBA implementation.
pub struct TextureDecoder;

impl TextureDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Decodes a texture using fallible allocations.
    pub fn decode(&self, texture: &Texture2D) -> Result<RgbaImage> {
        self.decode_with_budget(texture, None)
            .map_err(TextureDecodeFailure::into_binary)
    }

    pub(crate) fn decode_prepared(
        &self,
        width: u32,
        height: u32,
        format: TextureFormat,
        data: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<RgbaImage, TextureDecodeFailure> {
        self.decode_input(
            TextureDecodeInput {
                width,
                height,
                format,
                data,
            },
            Some(budget),
        )
    }

    fn decode_with_budget(
        &self,
        texture: &Texture2D,
        budget: Option<&mut AssetLoadBudget>,
    ) -> std::result::Result<RgbaImage, TextureDecodeFailure> {
        texture
            .validate()
            .map_err(|error| TextureDecodeFailure::Decode(BinaryError::invalid_data(error)))?;
        let width = u32::try_from(texture.width).map_err(|_| {
            TextureDecodeFailure::Decode(BinaryError::invalid_data("invalid texture width"))
        })?;
        let height = u32::try_from(texture.height).map_err(|_| {
            TextureDecodeFailure::Decode(BinaryError::invalid_data("invalid texture height"))
        })?;
        self.decode_input(
            TextureDecodeInput {
                width,
                height,
                format: texture.format,
                data: &texture.image_data,
            },
            budget,
        )
    }

    fn decode_input(
        &self,
        input: TextureDecodeInput<'_>,
        budget: Option<&mut AssetLoadBudget>,
    ) -> std::result::Result<RgbaImage, TextureDecodeFailure> {
        validate_dimensions(input.width, input.height).map_err(TextureDecodeFailure::Decode)?;
        if !self.can_decode(input.format) {
            return Err(TextureDecodeFailure::Decode(BinaryError::unsupported(
                format!("Unsupported texture format: {:?}", input.format),
            )));
        }
        let mut buffers =
            TextureDecodeBuffers::try_new(input.format, input.width, input.height, budget)?;

        let result = if input.format.is_crunch_compressed() {
            CrunchDecoder::new().decode(input, &mut buffers)
        } else if input.format.is_basic_format() {
            BasicDecoder::new().decode(input, &mut buffers)
        } else if input.format.is_compressed_format() {
            CompressedDecoder::new().decode(input, &mut buffers)
        } else if input.format.is_mobile_format() {
            MobileDecoder::new().decode(input, &mut buffers)
        } else {
            Err(BinaryError::unsupported(format!(
                "Unsupported texture format: {:?}",
                input.format
            )))
        };
        result.map_err(TextureDecodeFailure::Decode)?;
        buffers
            .into_image(input.width, input.height)
            .map_err(TextureDecodeFailure::Decode)
    }

    /// Returns whether this build has an implementation for the format.
    #[must_use]
    pub const fn can_decode(&self, format: TextureFormat) -> bool {
        match format {
            TextureFormat::Alpha8
            | TextureFormat::RGB24
            | TextureFormat::RGBA32
            | TextureFormat::ARGB32
            | TextureFormat::BGRA32
            | TextureFormat::RGBA4444
            | TextureFormat::ARGB4444
            | TextureFormat::RGB565 => true,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT1
            | TextureFormat::DXT5
            | TextureFormat::BC4
            | TextureFormat::BC5
            | TextureFormat::BC7
            | TextureFormat::ETC2_RGB
            | TextureFormat::ETC2_RGBA8
            | TextureFormat::ASTC_RGBA_4x4
            | TextureFormat::ASTC_RGBA_6x6
            | TextureFormat::ASTC_RGBA_8x8
            | TextureFormat::DXT1Crunched
            | TextureFormat::DXT5Crunched
            | TextureFormat::ETC_RGB4Crunched
            | TextureFormat::ETC2_RGBA8Crunched => true,
            _ => false,
        }
    }

    /// Returns the exact format inventory implemented by this build.
    #[must_use]
    pub fn supported_formats(&self) -> Vec<TextureFormat> {
        vec![
            TextureFormat::Alpha8,
            TextureFormat::RGB24,
            TextureFormat::RGBA32,
            TextureFormat::ARGB32,
            TextureFormat::BGRA32,
            TextureFormat::RGBA4444,
            TextureFormat::ARGB4444,
            TextureFormat::RGB565,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT1,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT5,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::BC4,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::BC5,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::BC7,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGB,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGBA8,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_4x4,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_6x6,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ASTC_RGBA_8x8,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT1Crunched,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::DXT5Crunched,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC_RGB4Crunched,
            #[cfg(feature = "texture-advanced")]
            TextureFormat::ETC2_RGBA8Crunched,
        ]
    }
}

#[derive(Clone, Copy)]
pub(super) struct TextureDecodeInput<'a> {
    width: u32,
    height: u32,
    format: TextureFormat,
    data: &'a [u8],
}

impl<'a> TextureDecodeInput<'a> {
    pub(super) const fn width(self) -> u32 {
        self.width
    }

    pub(super) const fn height(self) -> u32 {
        self.height
    }

    pub(super) const fn format(self) -> TextureFormat {
        self.format
    }

    pub(super) const fn data(self) -> &'a [u8] {
        self.data
    }
}

impl Default for TextureDecoder {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) enum TextureDecodeFailure {
    Decode(BinaryError),
    Budget(BudgetError),
    Allocation {
        resource: &'static str,
        requested: usize,
        source: TryReserveError,
    },
}

impl TextureDecodeFailure {
    fn into_binary(self) -> BinaryError {
        match self {
            Self::Decode(error) => error,
            Self::Budget(error) => BinaryError::Budget(error),
            Self::Allocation {
                resource,
                requested,
                source,
            } => BinaryError::allocation(resource, requested, source),
        }
    }
}

pub(super) struct TextureDecodeBuffers {
    rgba: Vec<u8>,
    #[cfg(feature = "texture-advanced")]
    words: Option<Vec<u32>>,
}

impl TextureDecodeBuffers {
    fn try_new(
        format: TextureFormat,
        width: u32,
        height: u32,
        mut budget: Option<&mut AssetLoadBudget>,
    ) -> std::result::Result<Self, TextureDecodeFailure> {
        let pixels = checked_pixel_count(width, height).map_err(TextureDecodeFailure::Decode)?;
        let rgba_length = pixels.checked_mul(4).ok_or_else(|| {
            TextureDecodeFailure::Decode(BinaryError::invalid_data(
                "decoded texture length overflows usize",
            ))
        })?;
        let rgba =
            reserve_fallible::<u8>(rgba_length, "texture RGBA output", budget.as_deref_mut())?;
        #[cfg(feature = "texture-advanced")]
        let words = if format.is_basic_format() {
            None
        } else {
            let mut words = reserve_fallible::<u32>(pixels, "texture decoder scratch", budget)?;
            words.resize(pixels, 0);
            Some(words)
        };
        #[cfg(not(feature = "texture-advanced"))]
        let _ = (format, budget);
        Ok(Self {
            rgba,
            #[cfg(feature = "texture-advanced")]
            words,
        })
    }

    pub(super) fn rgba_output(&mut self) -> &mut Vec<u8> {
        self.rgba.clear();
        &mut self.rgba
    }

    fn into_image(self, width: u32, height: u32) -> Result<RgbaImage> {
        create_rgba_image(self.rgba, width, height)
    }
}

fn reserve_fallible<T>(
    length: usize,
    resource: &'static str,
    mut budget: Option<&mut AssetLoadBudget>,
) -> std::result::Result<Vec<T>, TextureDecodeFailure> {
    let requested = length.checked_mul(size_of::<T>()).ok_or_else(|| {
        TextureDecodeFailure::Decode(BinaryError::invalid_data(format!(
            "{resource} length overflows usize"
        )))
    })?;
    let requested_u64 = u64::try_from(requested).map_err(|_| {
        TextureDecodeFailure::Decode(BinaryError::invalid_data(format!(
            "{resource} length exceeds the budget domain"
        )))
    })?;
    if let Some(budget) = budget.as_deref_mut() {
        budget
            .check_bytes(requested_u64)
            .map_err(TextureDecodeFailure::Budget)?;
    }

    let mut values = Vec::new();
    values
        .try_reserve_exact(length)
        .map_err(|source| TextureDecodeFailure::Allocation {
            resource,
            requested,
            source,
        })?;
    if let Some(budget) = budget {
        let retained = values
            .capacity()
            .checked_mul(size_of::<T>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| {
                TextureDecodeFailure::Decode(BinaryError::invalid_data(format!(
                    "{resource} capacity exceeds the budget domain"
                )))
            })?;
        budget
            .consume_bytes(retained)
            .map_err(TextureDecodeFailure::Budget)?;
    }
    Ok(values)
}

#[cfg(feature = "texture-advanced")]
pub(super) fn decode_word_output<E>(
    buffers: &mut TextureDecodeBuffers,
    operation: &'static str,
    decode: impl FnOnce(&mut [u32]) -> std::result::Result<(), E>,
    convert: impl Fn(u32) -> [u8; 4],
) -> Result<()>
where
    E: Display,
{
    {
        let words = buffers
            .words
            .as_deref_mut()
            .ok_or_else(|| BinaryError::invalid_data("compressed decoder has no scratch buffer"))?;
        decode(words)
            .map_err(|error| BinaryError::generic(format!("{operation} failed: {error}")))?;
    }
    let words = buffers
        .words
        .as_deref()
        .expect("word scratch was validated before conversion");
    let rgba = &mut buffers.rgba;
    rgba.clear();
    for &pixel in words {
        rgba.extend_from_slice(&convert(pixel));
    }
    Ok(())
}

/// `texture2ddecoder` stores pixels as BGRA bytes in a native `u32`.
#[cfg(feature = "texture-advanced")]
pub(super) const fn decoder_bgra_to_rgba(pixel: u32) -> [u8; 4] {
    let [blue, green, red, alpha] = pixel.to_le_bytes();
    [red, green, blue, alpha]
}

pub(super) fn checked_pixel_count(width: u32, height: u32) -> Result<usize> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| BinaryError::invalid_data("texture pixel count overflow"))?;
    usize::try_from(pixels)
        .map_err(|_| BinaryError::invalid_data("texture pixel count exceeds usize"))
}

pub(super) fn expected_source_length(
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> Result<usize> {
    checked_pixel_count(width, height)?
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| BinaryError::invalid_data("texture source length overflows usize"))
}

pub(super) fn source_prefix<'a>(
    data: &'a [u8],
    expected: usize,
    format: &'static str,
) -> Result<&'a [u8]> {
    data.get(..expected).ok_or_else(|| {
        BinaryError::invalid_data(format!(
            "Insufficient data for {format}: expected {expected}, got {}",
            data.len()
        ))
    })
}

fn create_rgba_image(data: Vec<u8>, width: u32, height: u32) -> Result<RgbaImage> {
    let expected = checked_pixel_count(width, height)?
        .checked_mul(4)
        .ok_or_else(|| BinaryError::invalid_data("RGBA image length overflows usize"))?;
    if data.len() != expected {
        return Err(BinaryError::invalid_data(format!(
            "Invalid RGBA data size: expected {expected}, got {}",
            data.len()
        )));
    }
    RgbaImage::from_raw(width, height, data)
        .ok_or_else(|| BinaryError::invalid_data("Failed to create RGBA image from raw data"))
}

pub(super) fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(BinaryError::invalid_data("Invalid texture dimensions"));
    }
    if width > 16_384 || height > 16_384 {
        return Err(BinaryError::invalid_data("Texture dimensions too large"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "texture-advanced")]
    #[test]
    fn texture2ddecoder_words_are_converted_from_bgra_to_rgba() {
        let pixel = u32::from_le_bytes([0x33, 0x22, 0x11, 0x44]);
        assert_eq!(decoder_bgra_to_rgba(pixel), [0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn failed_reservation_reaches_binary_error_without_a_string_bridge() {
        let error = reserve_fallible::<u8>(usize::MAX, "test allocation", None)
            .unwrap_err()
            .into_binary();

        assert!(matches!(
            error,
            BinaryError::Allocation {
                resource: "test allocation",
                requested: usize::MAX,
                ..
            }
        ));
    }
}
