//! Pixel-semantic regression tests for block-compressed texture decoders.

#![cfg(feature = "texture-advanced")]

use image::RgbaImage;
use unity_asset_decode::texture::{Texture2D, TextureDecoder, TextureFormat};

#[test]
fn dxt_decoders_preserve_red_and_alpha_channels() {
    let dxt1 = decode_block(
        TextureFormat::DXT1,
        vec![0x00, 0xf8, 0x00, 0xf8, 0, 0, 0, 0],
    );
    assert_solid_rgba(&dxt1, [255, 0, 0, 255]);

    let dxt5 = decode_block(
        TextureFormat::DXT5,
        vec![
            128, 128, 0, 0, 0, 0, 0, 0, // Alpha block.
            0x00, 0xf8, 0x00, 0xf8, 0, 0, 0, 0, // Red BC1 color block.
        ],
    );
    assert_solid_rgba(&dxt5, [255, 0, 0, 128]);
}

#[test]
fn bc4_decoder_uses_the_decoder_red_channel_as_luma() {
    let image = decode_block(TextureFormat::BC4, vec![123, 123, 0, 0, 0, 0, 0, 0]);

    assert_solid_rgba(&image, [123, 123, 123, 255]);
}

#[test]
fn bc5_decoder_preserves_red_and_green_channels() {
    let image = decode_block(
        TextureFormat::BC5,
        vec![
            225, 225, 0, 0, 0, 0, 0, 0, // Red channel block.
            71, 71, 0, 0, 0, 0, 0, 0, // Green channel block.
        ],
    );

    assert_solid_rgba(&image, [225, 71, 0, 255]);
}

fn decode_block(format: TextureFormat, image_data: Vec<u8>) -> RgbaImage {
    let data_size = i32::try_from(image_data.len()).expect("test block length fits i32");
    TextureDecoder::new()
        .decode(&Texture2D {
            width: 4,
            height: 4,
            complete_image_size: data_size,
            format,
            data_size,
            image_data,
            ..Texture2D::default()
        })
        .expect("known block-compressed fixture should decode")
}

fn assert_solid_rgba(image: &RgbaImage, expected: [u8; 4]) {
    assert_eq!(image.dimensions(), (4, 4));
    assert!(image.pixels().all(|pixel| pixel.0 == expected));
}
