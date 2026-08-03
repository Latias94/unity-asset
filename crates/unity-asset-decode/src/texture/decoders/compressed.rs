//! Desktop block-compressed texture decoders.

use super::{TextureDecodeBuffers, TextureDecodeInput};
#[cfg(feature = "texture-advanced")]
use super::{decode_word_output, decoder_bgra_to_rgba};
#[cfg(feature = "texture-advanced")]
use crate::texture::formats::TextureFormat;
use unity_asset_binary::{BinaryError, Result};

pub(super) struct CompressedDecoder;

impl CompressedDecoder {
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
                TextureFormat::DXT1 => decode_word_output(
                    buffers,
                    "DXT1 decoding",
                    |output| {
                        texture2ddecoder::decode_bc1(data, width as usize, height as usize, output)
                    },
                    decoder_bgra_to_rgba,
                ),
                TextureFormat::DXT5 => decode_word_output(
                    buffers,
                    "DXT5 decoding",
                    |output| {
                        texture2ddecoder::decode_bc3(data, width as usize, height as usize, output)
                    },
                    decoder_bgra_to_rgba,
                ),
                TextureFormat::BC7 => decode_word_output(
                    buffers,
                    "BC7 decoding",
                    |output| {
                        texture2ddecoder::decode_bc7(data, width as usize, height as usize, output)
                    },
                    decoder_bgra_to_rgba,
                ),
                TextureFormat::BC4 => decode_word_output(
                    buffers,
                    "BC4 decoding",
                    |output| {
                        texture2ddecoder::decode_bc4(data, width as usize, height as usize, output)
                    },
                    |pixel| {
                        let [red, _, _, _] = decoder_bgra_to_rgba(pixel);
                        [red, red, red, 255]
                    },
                ),
                TextureFormat::BC5 => decode_word_output(
                    buffers,
                    "BC5 decoding",
                    |output| {
                        texture2ddecoder::decode_bc5(data, width as usize, height as usize, output)
                    },
                    |pixel| {
                        let [red, green, _, _] = decoder_bgra_to_rgba(pixel);
                        [red, green, 0, 255]
                    },
                ),
                format => Err(BinaryError::unsupported(format!(
                    "Format {format:?} is not a desktop block-compressed texture format"
                ))),
            }
        }
        #[cfg(not(feature = "texture-advanced"))]
        {
            let _ = buffers;
            Err(BinaryError::unsupported(format!(
                "Compressed format {:?} requires texture-advanced feature",
                texture.format()
            )))
        }
    }
}

#[cfg(all(test, feature = "texture-advanced"))]
mod tests {
    use super::*;
    use crate::texture::{Texture2D, TextureDecoder};

    #[test]
    fn bc_decoders_preserve_red_and_green_channels() {
        let dxt1 = decode(
            TextureFormat::DXT1,
            vec![0x00, 0xf8, 0x00, 0xf8, 0, 0, 0, 0],
        );
        assert!(dxt1.pixels().all(|pixel| pixel.0 == [255, 0, 0, 255]));

        let bc4 = decode(TextureFormat::BC4, vec![255, 255, 0, 0, 0, 0, 0, 0]);
        assert!(bc4.pixels().all(|pixel| pixel.0 == [255, 255, 255, 255]));

        let mut bc5_source = vec![255, 255, 0, 0, 0, 0, 0, 0];
        bc5_source.extend([128, 128, 0, 0, 0, 0, 0, 0]);
        let bc5 = decode(TextureFormat::BC5, bc5_source);
        assert!(bc5.pixels().all(|pixel| pixel.0 == [255, 128, 0, 255]));
    }

    fn decode(format: TextureFormat, image_data: Vec<u8>) -> image::RgbaImage {
        TextureDecoder::new()
            .decode(&Texture2D {
                width: 4,
                height: 4,
                format,
                image_data,
                ..Texture2D::default()
            })
            .unwrap()
    }
}
