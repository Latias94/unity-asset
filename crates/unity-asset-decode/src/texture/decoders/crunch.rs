//! Crunch-compressed texture decoders.

use super::{TextureDecodeBuffers, TextureDecodeInput};
#[cfg(feature = "texture-advanced")]
use super::{decode_word_output, decoder_bgra_to_rgba};
#[cfg(not(feature = "texture-advanced"))]
use unity_asset_binary::BinaryError;
use unity_asset_binary::Result;

pub(super) struct CrunchDecoder;

impl CrunchDecoder {
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
            decode_word_output(
                buffers,
                "Crunch decompression",
                |output| {
                    texture2ddecoder::decode_crunch(
                        texture.data(),
                        width as usize,
                        height as usize,
                        output,
                    )
                },
                decoder_bgra_to_rgba,
            )
        }
        #[cfg(not(feature = "texture-advanced"))]
        {
            let _ = buffers;
            Err(BinaryError::unsupported(format!(
                "Crunch format {:?} requires texture-advanced feature",
                texture.format()
            )))
        }
    }
}
