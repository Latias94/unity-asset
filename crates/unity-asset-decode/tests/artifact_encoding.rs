#![cfg(all(feature = "audio", feature = "texture-advanced", feature = "sprite"))]

use image::{ImageFormat, RgbaImage};
use std::io::{self, Cursor, Write};
use unity_asset_core::AssetLoadBudget;
use unity_asset_decode::audio::{
    AudioClip, AudioCompressionFormat, AudioExporter, AudioSourceError, DecodedAudio,
    decode_audio_data,
};
use unity_asset_decode::sprite::{Sprite, SpriteProcessor};
use unity_asset_decode::texture::{Texture2D, TextureDecoder, TextureExporter, TextureFormat};
use unity_asset_decode::unity_version::UnityVersion;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const SHORT_VORBIS: &[u8] = include_bytes!("fixtures/short_vorbis.fsb");

#[test]
fn audio_writer_matches_path_exports() -> Result<(), Box<dyn std::error::Error>> {
    let audio = DecodedAudio::new(vec![-1.0, -0.25, 0.0, 0.5, 1.0, 0.125], 48_000, 2);
    let temp = tempfile::tempdir()?;

    let mut wav_sink = Cursor::new(Vec::new());
    AudioExporter::write_wav(&audio, &mut wav_sink)?;
    let wav_bytes = wav_sink.into_inner();
    assert_eq!(&wav_bytes[..4], b"RIFF");
    assert_eq!(&wav_bytes[8..12], b"WAVE");

    let wav_path = temp.path().join("artifact.wav");
    AudioExporter::export_wav(&audio, &wav_path)?;
    assert_eq!(std::fs::read(wav_path)?, wav_bytes);

    for bit_depth in [16, 32] {
        let mut pcm_sink = Cursor::new(Vec::new());
        AudioExporter::write_raw_pcm(&audio, &mut pcm_sink, bit_depth)?;

        let pcm_path = temp.path().join(format!("artifact-{bit_depth}.pcm"));
        AudioExporter::export_raw_pcm(&audio, &pcm_path, bit_depth)?;
        assert_eq!(std::fs::read(pcm_path)?, pcm_sink.into_inner());
    }

    Ok(())
}

#[test]
fn standard_audio_source_rejects_headerless_pcm_and_adpcm() {
    for format in [AudioCompressionFormat::PCM, AudioCompressionFormat::ADPCM] {
        let clip = AudioClip::new("not-a-wave".into(), format);
        let error = AudioExporter::prepare_standard_source(
            &clip,
            &[1, 2, 3, 4],
            &mut AssetLoadBudget::default(),
        )
        .err()
        .expect("headerless PCM-like bytes must not be published as WAV");
        assert!(matches!(error, AudioSourceError::UnsupportedFormat(actual) if actual == format));
    }
}

#[test]
fn prepared_ogg_passthrough_is_exact_and_preserves_output_errors() {
    let clip = AudioClip::new("direct".into(), AudioCompressionFormat::Vorbis);
    let source = rebuilt_playable_ogg();
    assert!(
        decode_audio_data(AudioCompressionFormat::Vorbis, source.clone()).is_ok(),
        "the direct Ogg fixture must be playable Vorbis"
    );
    let prepared =
        AudioExporter::prepare_standard_source(&clip, &source, &mut AssetLoadBudget::default())
            .unwrap();

    let mut output = Vec::new();
    prepared.write_to(&source, &mut output).unwrap();
    assert_eq!(output, source);

    let error = prepared
        .write_to(&source, &mut RejectingWriter)
        .unwrap_err();
    assert!(matches!(error, AudioSourceError::Output(_)));

    let mut corrupt = source;
    corrupt[22] ^= 0xff;
    let Err(error) =
        AudioExporter::prepare_standard_source(&clip, &corrupt, &mut AssetLoadBudget::default())
    else {
        panic!("a corrupt Ogg checksum must not be published as a direct audio source");
    };
    assert!(matches!(error, AudioSourceError::InvalidData(_)));
}

#[test]
fn standard_audio_source_rejects_crc_correct_non_vorbis_ogg() {
    let clip = AudioClip::new("not-vorbis".into(), AudioCompressionFormat::Vorbis);
    let source = crc_correct_non_vorbis_ogg();
    let Err(error) =
        AudioExporter::prepare_standard_source(&clip, &source, &mut AssetLoadBudget::default())
    else {
        panic!("a non-Vorbis Ogg container must not be accepted as direct audio");
    };
    assert!(matches!(error, AudioSourceError::InvalidData(_)));
}

#[test]
fn rebuilt_fsb5_vorbis_decodes_to_its_declared_frame_count() {
    let clip = AudioClip::new("short".into(), AudioCompressionFormat::Vorbis);
    let prepared = AudioExporter::prepare_standard_source(
        &clip,
        SHORT_VORBIS,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let mut output = Vec::new();
    prepared.write_to(SHORT_VORBIS, &mut output).unwrap();

    let decoded = decode_audio_data(AudioCompressionFormat::Vorbis, output).unwrap();
    assert_eq!(decoded.frame_count(), 24_806);
}

fn rebuilt_playable_ogg() -> Vec<u8> {
    let clip = AudioClip::new("short".into(), AudioCompressionFormat::Vorbis);
    let prepared = AudioExporter::prepare_standard_source(
        &clip,
        SHORT_VORBIS,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let mut output = Vec::new();
    prepared.write_to(SHORT_VORBIS, &mut output).unwrap();
    output
}

fn crc_correct_non_vorbis_ogg() -> Vec<u8> {
    let mut page = vec![0_u8; 28];
    page[..4].copy_from_slice(b"OggS");
    page[5] = 0x06;
    page[26] = 1;
    page[27] = 0;
    let checksum = ogg_checksum(&page);
    page[22..26].copy_from_slice(&checksum.to_le_bytes());
    page
}

fn ogg_checksum(page: &[u8]) -> u32 {
    const POLYNOMIAL: u32 = 0x04C1_1DB7;

    let mut checksum = 0_u32;
    for (index, byte) in page.iter().copied().enumerate() {
        let byte = if (22..26).contains(&index) { 0 } else { byte };
        checksum ^= u32::from(byte) << 24;
        for _ in 0..8 {
            checksum = if checksum & 0x8000_0000 != 0 {
                (checksum << 1) ^ POLYNOMIAL
            } else {
                checksum << 1
            };
        }
    }
    checksum
}

struct RejectingWriter;

impl Write for RejectingWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("fixture output failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn texture_png_writer_matches_path_export() -> Result<(), Box<dyn std::error::Error>> {
    let image = RgbaImage::from_raw(
        2,
        2,
        vec![
            255, 0, 0, 255, 0, 255, 0, 192, 0, 0, 255, 128, 255, 255, 255, 64,
        ],
    )
    .expect("fixture dimensions must match its RGBA payload");

    let mut png_sink = Cursor::new(Vec::new());
    TextureExporter::write_png(&image, &mut png_sink)?;
    let png_bytes = png_sink.into_inner();
    assert_eq!(&png_bytes[..PNG_SIGNATURE.len()], PNG_SIGNATURE);
    let decoded = image::load_from_memory_with_format(&png_bytes, ImageFormat::Png)?.to_rgba8();
    assert_eq!(decoded.dimensions(), image.dimensions());
    assert_eq!(decoded.as_raw(), image.as_raw());

    let temp = tempfile::tempdir()?;
    let png_path = temp.path().join("artifact.png");
    TextureExporter::export_png(&image, &png_path)?;
    assert_eq!(std::fs::read(png_path)?, png_bytes);

    Ok(())
}

#[test]
fn sprite_vec_export_delegates_to_png_writer() -> Result<(), Box<dyn std::error::Error>> {
    let texture = Texture2D {
        width: 2,
        height: 2,
        format: TextureFormat::RGBA32,
        image_data: vec![
            255, 0, 0, 255, 0, 255, 0, 192, 0, 0, 255, 128, 255, 255, 255, 64,
        ],
        ..Default::default()
    };
    let sprite = Sprite {
        rect_width: 2.0,
        rect_height: 1.0,
        ..Default::default()
    };
    let version = UnityVersion::parse_version("2020.3.12f1")?;
    let processor = SpriteProcessor::new(version);

    let png_bytes = processor.extract_sprite_image(&sprite, &texture)?;
    let mut png_sink = Cursor::new(Vec::new());
    processor.write_sprite_png(&sprite, &texture, &mut png_sink)?;

    assert_eq!(&png_bytes[..PNG_SIGNATURE.len()], PNG_SIGNATURE);
    assert_eq!(png_sink.into_inner(), png_bytes);
    let decoded = image::load_from_memory_with_format(&png_bytes, ImageFormat::Png)?.to_rgba8();
    assert_eq!(decoded.dimensions(), (2, 1));
    assert_eq!(decoded.as_raw(), &[0, 0, 255, 128, 255, 255, 255, 64]);
    Ok(())
}

#[test]
fn astc_6x6_is_advertised_with_its_decoder_geometry() {
    let format = TextureFormat::ASTC_RGBA_6x6;
    let info = format.info();

    assert!(info.supported);
    assert_eq!(info.block_size, (6, 6));
    assert_eq!(format.calculate_data_size(492, 180), 39_360);
    assert!(TextureDecoder::new().can_decode(format));
}
