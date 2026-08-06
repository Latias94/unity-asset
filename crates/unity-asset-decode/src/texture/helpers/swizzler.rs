//! Texture swizzling utilities
//!
//! This module provides functionality for texture data manipulation and channel swizzling.
//! Inspired by UnityPy's TextureSwizzler.

use image::RgbaImage;
use unity_asset_binary::{BinaryError, Result};

/// Texture swizzling utility
///
/// This struct provides methods for manipulating texture data,
/// including channel swizzling and data transformation.
pub struct TextureSwizzler;

impl TextureSwizzler {
    const SWITCH_GOB_WIDTH_UNITS: usize = 4;
    const SWITCH_GOB_HEIGHT_UNITS: usize = 8;
    const SWITCH_STORAGE_UNIT_BYTES: usize = 16;

    /// Swap red and blue channels (RGBA -> BGRA or vice versa)
    pub fn swap_rb_channels(image: &mut RgbaImage) {
        for pixel in image.pixels_mut() {
            let temp = pixel[0]; // Store R
            pixel[0] = pixel[2]; // R = B
            pixel[2] = temp; // B = R
        }
    }

    /// Converts Unity's bottom-left row order into the top-left image convention in place.
    pub(crate) fn normalize_top_left(image: &mut RgbaImage) {
        image::imageops::flip_vertical_in_place(image);
    }

    /// Converts one proven, edge-aligned Nintendo Switch block-linear surface to linear bytes.
    pub(crate) fn deswizzle_switch_block_linear(
        source: &[u8],
        output: &mut [u8],
        width: u32,
        height: u32,
        block_width: u8,
        block_height: u8,
        gobs_per_block: u8,
    ) -> Result<()> {
        let block_width = usize::from(block_width);
        let block_height = usize::from(block_height);
        let gobs_per_block = usize::from(gobs_per_block);
        let width = usize::try_from(width)
            .map_err(|_| BinaryError::invalid_data("Switch texture width exceeds usize"))?;
        let height = usize::try_from(height)
            .map_err(|_| BinaryError::invalid_data("Switch texture height exceeds usize"))?;
        if block_width == 0 || block_height == 0 || gobs_per_block == 0 {
            return Err(BinaryError::invalid_data(
                "Switch texture storage geometry contains zero",
            ));
        }
        if width % block_width != 0 || height % block_height != 0 {
            return Err(BinaryError::invalid_data(
                "Switch texture dimensions are not block aligned",
            ));
        }
        let units_x = width / block_width;
        let units_y = height / block_height;
        let block_rows = Self::SWITCH_GOB_HEIGHT_UNITS
            .checked_mul(gobs_per_block)
            .ok_or_else(|| BinaryError::invalid_data("Switch block height overflows usize"))?;
        if units_x % Self::SWITCH_GOB_WIDTH_UNITS != 0 || units_y % block_rows != 0 {
            return Err(BinaryError::invalid_data(
                "Switch texture is not aligned to complete GOB blocks",
            ));
        }
        let expected = units_x
            .checked_mul(units_y)
            .and_then(|units| units.checked_mul(Self::SWITCH_STORAGE_UNIT_BYTES))
            .ok_or_else(|| BinaryError::invalid_data("Switch texture storage size overflows"))?;
        if source.len() != expected || output.len() != expected {
            return Err(BinaryError::invalid_data(
                "Switch texture storage length does not match its geometry",
            ));
        }

        let mut source_offset = 0_usize;
        for block_y in 0..(units_y / block_rows) {
            for block_x in 0..(units_x / Self::SWITCH_GOB_WIDTH_UNITS) {
                let base_x = block_x * Self::SWITCH_GOB_WIDTH_UNITS;
                for gob_y in 0..gobs_per_block {
                    let base_y = (block_y * gobs_per_block + gob_y) * Self::SWITCH_GOB_HEIGHT_UNITS;
                    for ordinal in 0..32_usize {
                        let local_x = ((ordinal >> 3) & 0b10) | ((ordinal >> 1) & 0b1);
                        let local_y = ((ordinal >> 1) & 0b110) | (ordinal & 0b1);
                        let destination = (base_y + local_y)
                            .checked_mul(units_x)
                            .and_then(|offset| offset.checked_add(base_x + local_x))
                            .and_then(|offset| offset.checked_mul(Self::SWITCH_STORAGE_UNIT_BYTES))
                            .ok_or_else(|| {
                                BinaryError::invalid_data("Switch destination offset overflows")
                            })?;
                        let source_end = source_offset
                            .checked_add(Self::SWITCH_STORAGE_UNIT_BYTES)
                            .ok_or_else(|| {
                                BinaryError::invalid_data("Switch source offset overflows")
                            })?;
                        let destination_end = destination
                            .checked_add(Self::SWITCH_STORAGE_UNIT_BYTES)
                            .ok_or_else(|| {
                                BinaryError::invalid_data("Switch destination extent overflows")
                            })?;
                        output
                            .get_mut(destination..destination_end)
                            .ok_or_else(|| {
                                BinaryError::invalid_data("Switch destination exceeds output")
                            })?
                            .copy_from_slice(source.get(source_offset..source_end).ok_or_else(
                                || BinaryError::invalid_data("Switch source ends inside a GOB"),
                            )?);
                        source_offset = source_end;
                    }
                }
            }
        }
        if source_offset != source.len() {
            return Err(BinaryError::invalid_data(
                "Switch deswizzle did not consume the complete source",
            ));
        }
        Ok(())
    }

    /// Flip image horizontally
    pub fn flip_horizontal(image: &RgbaImage) -> RgbaImage {
        let (width, height) = image.dimensions();
        let mut flipped = RgbaImage::new(width, height);

        for y in 0..height {
            for x in 0..width {
                let src_pixel = image.get_pixel(x, y);
                flipped.put_pixel(width - 1 - x, y, *src_pixel);
            }
        }

        flipped
    }

    /// Apply gamma correction
    pub fn apply_gamma(image: &mut RgbaImage, gamma: f32) {
        let inv_gamma = 1.0 / gamma;

        for pixel in image.pixels_mut() {
            // Apply gamma to RGB channels, leave alpha unchanged
            for i in 0..3 {
                let normalized = pixel[i] as f32 / 255.0;
                let corrected = normalized.powf(inv_gamma);
                pixel[i] = (corrected * 255.0).clamp(0.0, 255.0) as u8;
            }
        }
    }

    /// Convert to grayscale using luminance formula
    pub fn to_grayscale(image: &RgbaImage) -> RgbaImage {
        let (width, height) = image.dimensions();
        let mut gray = RgbaImage::new(width, height);

        for (x, y, pixel) in image.enumerate_pixels() {
            // Use standard luminance formula
            let luminance =
                (0.299 * pixel[0] as f32 + 0.587 * pixel[1] as f32 + 0.114 * pixel[2] as f32) as u8;

            gray.put_pixel(
                x,
                y,
                image::Rgba([luminance, luminance, luminance, pixel[3]]),
            );
        }

        gray
    }

    /// Premultiply alpha
    pub fn premultiply_alpha(image: &mut RgbaImage) {
        for pixel in image.pixels_mut() {
            let alpha = pixel[3] as f32 / 255.0;

            // Premultiply RGB channels by alpha
            for i in 0..3 {
                pixel[i] = (pixel[i] as f32 * alpha) as u8;
            }
        }
    }

    /// Unpremultiply alpha
    pub fn unpremultiply_alpha(image: &mut RgbaImage) {
        for pixel in image.pixels_mut() {
            let alpha = pixel[3] as f32 / 255.0;

            if alpha > 0.0 {
                // Unpremultiply RGB channels by alpha
                for i in 0..3 {
                    pixel[i] = ((pixel[i] as f32 / alpha).clamp(0.0, 255.0)) as u8;
                }
            }
        }
    }

    /// Apply channel mask (set specific channels to 0 or 255)
    pub fn apply_channel_mask(image: &mut RgbaImage, mask: [Option<u8>; 4]) {
        for pixel in image.pixels_mut() {
            for (i, &mask_value) in mask.iter().enumerate() {
                if let Some(value) = mask_value {
                    pixel[i] = value;
                }
            }
        }
    }

    /// Extract single channel as grayscale image
    pub fn extract_channel(image: &RgbaImage, channel: usize) -> Result<RgbaImage> {
        if channel >= 4 {
            return Err(BinaryError::invalid_data("Channel index must be 0-3"));
        }

        let (width, height) = image.dimensions();
        let mut result = RgbaImage::new(width, height);

        for (x, y, pixel) in image.enumerate_pixels() {
            let value = pixel[channel];
            result.put_pixel(x, y, image::Rgba([value, value, value, 255]));
        }

        Ok(result)
    }

    /// Combine channels from different images
    pub fn combine_channels(
        r_image: Option<&RgbaImage>,
        g_image: Option<&RgbaImage>,
        b_image: Option<&RgbaImage>,
        a_image: Option<&RgbaImage>,
    ) -> Result<RgbaImage> {
        // Get dimensions from the first available image
        let (width, height) = [r_image, g_image, b_image, a_image]
            .iter()
            .find_map(|img| img.map(|i| i.dimensions()))
            .ok_or_else(|| BinaryError::invalid_data("At least one image must be provided"))?;

        let mut result = RgbaImage::new(width, height);

        for (x, y, pixel) in result.enumerate_pixels_mut() {
            pixel[0] = r_image.map_or(0, |img| img.get_pixel(x, y)[0]);
            pixel[1] = g_image.map_or(0, |img| img.get_pixel(x, y)[0]);
            pixel[2] = b_image.map_or(0, |img| img.get_pixel(x, y)[0]);
            pixel[3] = a_image.map_or(255, |img| img.get_pixel(x, y)[0]);
        }

        Ok(result)
    }

    /// Resize image using nearest neighbor (for pixel art)
    pub fn resize_nearest(image: &RgbaImage, new_width: u32, new_height: u32) -> RgbaImage {
        let (old_width, old_height) = image.dimensions();
        let mut result = RgbaImage::new(new_width, new_height);

        for (x, y, pixel) in result.enumerate_pixels_mut() {
            let src_x = (x * old_width / new_width).min(old_width - 1);
            let src_y = (y * old_height / new_height).min(old_height - 1);
            *pixel = *image.get_pixel(src_x, src_y);
        }

        result
    }

    /// Apply Unity-specific texture corrections
    pub fn apply_unity_corrections(image: &mut RgbaImage, flip_y: bool, swap_rb: bool) {
        if swap_rb {
            Self::swap_rb_channels(image);
        }

        if flip_y {
            Self::normalize_top_left(image);
        }
    }
}
