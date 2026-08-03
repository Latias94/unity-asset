//! Mobile block-compressed texture decoders.

use super::{TextureDecodeBuffers, TextureDecodeInput};
#[cfg(feature = "texture-advanced")]
use super::{decode_word_output, decoder_bgra_to_rgba};
#[cfg(feature = "texture-advanced")]
use crate::texture::formats::TextureFormat;
use unity_asset_binary::{BinaryError, Result};

pub(super) struct MobileDecoder;

impl MobileDecoder {
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

        #[cfg(feature = "texture-advanced")]
        {
            let data = texture.data();
            match texture.format() {
                TextureFormat::ETC2_RGB => decode_word_output(
                    buffers,
                    "ETC2 RGB decoding",
                    |output| {
                        texture2ddecoder::decode_etc2_rgb(
                            data,
                            width as usize,
                            height as usize,
                            output,
                        )
                    },
                    |pixel| {
                        let [red, green, blue, _] = decoder_bgra_to_rgba(pixel);
                        [red, green, blue, 255]
                    },
                ),
                TextureFormat::ETC2_RGBA8 => decode_word_output(
                    buffers,
                    "ETC2 RGBA8 decoding",
                    |output| {
                        texture2ddecoder::decode_etc2_rgba8(
                            data,
                            width as usize,
                            height as usize,
                            output,
                        )
                    },
                    decoder_bgra_to_rgba,
                ),
                TextureFormat::ASTC_RGBA_4x4 => {
                    decode_astc(buffers, data, width, height, 4, 4, "ASTC 4x4 decoding")
                }
                TextureFormat::ASTC_RGBA_6x6 => {
                    decode_astc(buffers, data, width, height, 6, 6, "ASTC 6x6 decoding")
                }
                TextureFormat::ASTC_RGBA_8x8 => {
                    decode_astc(buffers, data, width, height, 8, 8, "ASTC 8x8 decoding")
                }
                format => Err(BinaryError::unsupported(format!(
                    "Format {format:?} is not a supported mobile texture format"
                ))),
            }
        }
        #[cfg(not(feature = "texture-advanced"))]
        {
            let _ = buffers;
            Err(BinaryError::unsupported(format!(
                "Mobile format {:?} requires texture-advanced feature",
                texture.format()
            )))
        }
    }
}

#[cfg(feature = "texture-advanced")]
fn decode_astc(
    buffers: &mut TextureDecodeBuffers,
    data: &[u8],
    width: u32,
    height: u32,
    block_width: usize,
    block_height: usize,
    operation: &'static str,
) -> Result<()> {
    decode_word_output(
        buffers,
        operation,
        |output| {
            texture2ddecoder::decode_astc(
                data,
                width as usize,
                height as usize,
                block_width,
                block_height,
                output,
            )
        },
        decoder_bgra_to_rgba,
    )
}
