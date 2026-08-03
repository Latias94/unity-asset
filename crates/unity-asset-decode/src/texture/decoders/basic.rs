//! Uncompressed texture decoders.

use super::{TextureDecodeBuffers, TextureDecodeInput, expected_source_length, source_prefix};
use crate::texture::formats::TextureFormat;
use unity_asset_binary::{BinaryError, Result};

pub(super) struct BasicDecoder;

impl BasicDecoder {
    pub(super) const fn new() -> Self {
        Self
    }

    pub(super) fn decode(
        &self,
        texture: TextureDecodeInput<'_>,
        buffers: &mut TextureDecodeBuffers,
    ) -> Result<()> {
        let width = texture.width();
        let height = texture.height();
        let source = texture.data();
        let output = buffers.rgba_output();

        match texture.format() {
            TextureFormat::RGBA32 => {
                let expected = expected_source_length(width, height, 4)?;
                output.extend_from_slice(source_prefix(source, expected, "RGBA32")?);
            }
            TextureFormat::RGB24 => {
                let expected = expected_source_length(width, height, 3)?;
                for pixel in source_prefix(source, expected, "RGB24")?.chunks_exact(3) {
                    output.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
                }
            }
            TextureFormat::ARGB32 => {
                let expected = expected_source_length(width, height, 4)?;
                for pixel in source_prefix(source, expected, "ARGB32")?.chunks_exact(4) {
                    output.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
                }
            }
            TextureFormat::BGRA32 => {
                let expected = expected_source_length(width, height, 4)?;
                for pixel in source_prefix(source, expected, "BGRA32")?.chunks_exact(4) {
                    output.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                }
            }
            TextureFormat::Alpha8 => {
                let expected = expected_source_length(width, height, 1)?;
                for &alpha in source_prefix(source, expected, "Alpha8")? {
                    output.extend_from_slice(&[255, 255, 255, alpha]);
                }
            }
            TextureFormat::RGBA4444 => {
                let expected = expected_source_length(width, height, 2)?;
                for bytes in source_prefix(source, expected, "RGBA4444")?.chunks_exact(2) {
                    let pixel = u16::from_le_bytes([bytes[0], bytes[1]]);
                    let r = ((pixel >> 12) & 0x0f) as u8;
                    let g = ((pixel >> 8) & 0x0f) as u8;
                    let b = ((pixel >> 4) & 0x0f) as u8;
                    let a = (pixel & 0x0f) as u8;
                    output.extend_from_slice(&[
                        (r << 4) | r,
                        (g << 4) | g,
                        (b << 4) | b,
                        (a << 4) | a,
                    ]);
                }
            }
            TextureFormat::ARGB4444 => {
                let expected = expected_source_length(width, height, 2)?;
                for bytes in source_prefix(source, expected, "ARGB4444")?.chunks_exact(2) {
                    let pixel = u16::from_le_bytes([bytes[0], bytes[1]]);
                    let a = ((pixel >> 12) & 0x0f) as u8;
                    let r = ((pixel >> 8) & 0x0f) as u8;
                    let g = ((pixel >> 4) & 0x0f) as u8;
                    let b = (pixel & 0x0f) as u8;
                    output.extend_from_slice(&[
                        (r << 4) | r,
                        (g << 4) | g,
                        (b << 4) | b,
                        (a << 4) | a,
                    ]);
                }
            }
            TextureFormat::RGB565 => {
                let expected = expected_source_length(width, height, 2)?;
                for bytes in source_prefix(source, expected, "RGB565")?.chunks_exact(2) {
                    let pixel = u16::from_le_bytes([bytes[0], bytes[1]]);
                    let r = ((pixel >> 11) & 0x1f) as u8;
                    let g = ((pixel >> 5) & 0x3f) as u8;
                    let b = (pixel & 0x1f) as u8;
                    output.extend_from_slice(&[
                        (r << 3) | (r >> 2),
                        (g << 2) | (g >> 4),
                        (b << 3) | (b >> 2),
                        255,
                    ]);
                }
            }
            format => {
                return Err(BinaryError::unsupported(format!(
                    "Format {format:?} is not an uncompressed texture format"
                )));
            }
        }
        Ok(())
    }
}
