#![cfg(all(feature = "audio", feature = "texture-advanced", feature = "sprite"))]

use image::{GenericImageView, ImageFormat, RgbaImage};
use std::io::{self, Cursor, Seek, SeekFrom, Write};
use unity_asset_binary::BinaryError;
use unity_asset_core::AssetLoadBudget;
use unity_asset_decode::audio::{
    AudioClip, AudioCompressionFormat, AudioExporter, AudioSourceError, DecodedAudio,
    PreparedAudioSource, decode_audio_data,
};
use unity_asset_decode::media::BudgetedMediaBytes;
use unity_asset_decode::sprite::{Sprite, SpriteProcessor};
use unity_asset_decode::texture::{Texture2D, TextureDecoder, TextureExporter, TextureFormat};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const SHORT_VORBIS: &[u8] = include_bytes!("fixtures/short_vorbis.fsb");

fn prepare_standard_source(
    clip: &AudioClip,
    bytes: Vec<u8>,
) -> Result<PreparedAudioSource, AudioSourceError> {
    let mut budget = AssetLoadBudget::default();
    let source = BudgetedMediaBytes::from_vec(bytes, "test audio source", &mut budget)?;
    AudioExporter::prepare_standard_source(clip, source, &mut budget)
}

#[test]
fn audio_writers_encode_caller_owned_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let audio = DecodedAudio::new(vec![-1.0, -0.25, 0.0, 0.5, 1.0, 0.125], 48_000, 2);

    let mut wav_sink = Cursor::new(Vec::new());
    AudioExporter::write_wav(&audio, &mut wav_sink)?;
    let wav_bytes = wav_sink.into_inner();
    assert_eq!(&wav_bytes[..4], b"RIFF");
    assert_eq!(&wav_bytes[8..12], b"WAVE");

    for bit_depth in [16, 32] {
        let mut pcm_sink = Cursor::new(Vec::new());
        AudioExporter::write_raw_pcm(&audio, &mut pcm_sink, bit_depth)?;
        let bytes_per_sample = usize::from(bit_depth) / 8;
        assert_eq!(
            pcm_sink.into_inner().len(),
            audio.samples.len() * bytes_per_sample
        );
    }

    Ok(())
}

#[test]
fn wav_rejects_incomplete_channel_frames_before_writing() {
    let audio = DecodedAudio::new(vec![0.25], 48_000, 2);
    let mut sink = Vec::new();

    let error = AudioExporter::write_wav(&audio, &mut sink).unwrap_err();

    assert!(matches!(error, BinaryError::InvalidData(_)));
    assert!(sink.is_empty());
}

#[test]
fn standard_audio_source_rejects_headerless_pcm_and_adpcm() {
    for format in [AudioCompressionFormat::PCM, AudioCompressionFormat::ADPCM] {
        let clip = AudioClip::new("not-a-wave".into(), format);
        let error = prepare_standard_source(&clip, vec![1, 2, 3, 4])
            .err()
            .expect("headerless PCM-like bytes must not be published as WAV");
        assert!(matches!(error, AudioSourceError::InvalidData(_)));
    }
}

#[test]
fn standard_audio_source_rejects_header_only_containers() {
    let cases = [
        (
            AudioCompressionFormat::PCM,
            b"RIFF\x04\0\0\0WAVE".as_slice(),
        ),
        (
            AudioCompressionFormat::MP3,
            b"ID3\x04\0\0\0\0\0\0".as_slice(),
        ),
        (
            AudioCompressionFormat::AAC,
            b"\xff\xf1\x50\x80\0\xff\xfc".as_slice(),
        ),
    ];

    for (format, bytes) in cases {
        let clip = AudioClip::new("header-only".into(), format);
        let Err(error) = prepare_standard_source(&clip, bytes.to_vec()) else {
            panic!("a header without playable frames must be rejected");
        };

        assert!(match error {
            AudioSourceError::InvalidData(_) => true,
            AudioSourceError::UnsupportedFormat(actual) => actual == format,
            _ => false,
        });
    }
}

#[test]
fn standard_audio_source_rejects_reserved_adts_sample_rates() {
    for sampling_frequency_index in [13_u8, 14] {
        let bytes = [
            0xFF,
            0xF1,
            0x40 | (sampling_frequency_index << 2),
            0x80,
            0x01,
            0x1F,
            0xFC,
            0x00,
        ];
        let clip = AudioClip::new("reserved-adts-rate".into(), AudioCompressionFormat::AAC);

        let result = prepare_standard_source(&clip, bytes.to_vec());

        assert!(matches!(result, Err(AudioSourceError::InvalidData(_))));
    }
}

#[test]
fn standard_audio_source_rejects_adpcm_without_codec_extension() {
    let bytes = wave_fixture(0x11, 1, 8_000, 8_000, 4, 4, &[], &[0; 4]);
    let clip = AudioClip::new("invalid-adpcm".into(), AudioCompressionFormat::ADPCM);

    let result = prepare_standard_source(&clip, bytes.to_vec());

    assert!(matches!(result, Err(AudioSourceError::InvalidData(_))));
}

#[test]
fn standard_audio_source_accepts_complete_minimal_containers() {
    let mut wav = Vec::new();
    AudioExporter::write_wav(&DecodedAudio::new(vec![0.0], 8_000, 1), &mut wav).unwrap();

    let mut mp3 = vec![0_u8; 417];
    mp3[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);

    let aac = vec![0xFF, 0xF1, 0x50, 0x80, 0x01, 0x1F, 0xFC, 0x00];
    let ima_adpcm = wave_fixture(0x11, 1, 8_000, 7_111, 8, 4, &[2, 0, 9, 0], &[0; 8]);

    for (format, bytes) in [
        (AudioCompressionFormat::PCM, wav),
        (AudioCompressionFormat::ADPCM, ima_adpcm),
        (AudioCompressionFormat::MP3, mp3),
        (AudioCompressionFormat::AAC, aac),
    ] {
        let clip = AudioClip::new("complete".into(), format);
        let expected = bytes.clone();
        let prepared = prepare_standard_source(&clip, bytes).unwrap();
        let mut output = Vec::new();
        prepared.write_to(&mut output).unwrap();
        assert_eq!(output, expected);
    }
}

fn wave_fixture(
    format: u16,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
    extra: &[u8],
    data: &[u8],
) -> Vec<u8> {
    let fmt_size = 16_u32 + u32::try_from(extra.len()).unwrap();
    let riff_size = 4_u32 + 8 + fmt_size + 8 + u32::try_from(data.len()).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&riff_size.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&fmt_size.to_le_bytes());
    bytes.extend_from_slice(&format.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&bits_per_sample.to_le_bytes());
    bytes.extend_from_slice(extra);
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(data);
    bytes
}

#[test]
fn direct_ogg_requires_a_budgeted_setup_validator_before_passthrough() {
    let clip = AudioClip::new("direct".into(), AudioCompressionFormat::Vorbis);
    let source = rebuilt_playable_ogg();
    assert!(
        decode_audio_data(AudioCompressionFormat::Vorbis, source.clone()).is_ok(),
        "the direct Ogg fixture must be playable Vorbis"
    );
    let Err(error) = prepare_standard_source(&clip, source.clone()) else {
        panic!("direct Ogg passthrough must remain disabled without setup validation");
    };
    assert!(matches!(
        error,
        AudioSourceError::UnsupportedContainer {
            format: AudioCompressionFormat::Vorbis,
            container: "Ogg Vorbis",
        }
    ));

    let mut corrupt = source;
    corrupt[22] ^= 0xff;
    let Err(error) = prepare_standard_source(&clip, corrupt) else {
        panic!("a corrupt Ogg checksum must not be published as a direct audio source");
    };
    assert!(matches!(error, AudioSourceError::InvalidData(_)));
}

#[test]
fn standard_audio_source_rejects_crc_correct_non_vorbis_ogg() {
    let clip = AudioClip::new("not-vorbis".into(), AudioCompressionFormat::Vorbis);
    let source = crc_correct_non_vorbis_ogg();
    let Err(error) = prepare_standard_source(&clip, source) else {
        panic!("a non-Vorbis Ogg container must not be accepted as direct audio");
    };
    assert!(matches!(
        error,
        AudioSourceError::UnsupportedContainer {
            format: AudioCompressionFormat::Vorbis,
            container: "Ogg Vorbis",
        }
    ));
}

#[test]
fn rebuilt_fsb5_vorbis_decodes_to_its_declared_frame_count() {
    let clip = AudioClip::new("short".into(), AudioCompressionFormat::Vorbis);
    let prepared = prepare_standard_source(&clip, SHORT_VORBIS.to_vec()).unwrap();
    let mut output = Vec::new();
    prepared.write_to(&mut output).unwrap();

    let decoded = decode_audio_data(AudioCompressionFormat::Vorbis, output).unwrap();
    assert_eq!(decoded.frame_count(), 24_806);
}

#[test]
fn strict_fsb5_prepare_rejects_truncated_sections_and_damaged_lengths() {
    const HEADER_LEN: usize = 0x3C;
    const SAMPLE_HEADERS_LEN_OFFSET: usize = 12;
    const NAMES_LEN_OFFSET: usize = 16;
    const DATA_LEN_OFFSET: usize = 20;

    fn declared_len(bytes: &[u8], offset: usize) -> usize {
        usize::try_from(u32::from_le_bytes(
            bytes[offset..offset + 4].try_into().unwrap(),
        ))
        .unwrap()
    }

    fn increment_declared_len(bytes: &mut [u8], offset: usize) {
        let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        bytes[offset..offset + 4].copy_from_slice(&length.checked_add(1).unwrap().to_le_bytes());
    }

    let sample_headers_end = HEADER_LEN + declared_len(SHORT_VORBIS, SAMPLE_HEADERS_LEN_OFFSET);
    let data_start = sample_headers_end + declared_len(SHORT_VORBIS, NAMES_LEN_OFFSET);
    let data_end = data_start + declared_len(SHORT_VORBIS, DATA_LEN_OFFSET);
    assert_eq!(data_end, SHORT_VORBIS.len());

    let mut oversized_sample_headers = SHORT_VORBIS.to_vec();
    increment_declared_len(&mut oversized_sample_headers, SAMPLE_HEADERS_LEN_OFFSET);
    let mut oversized_data = SHORT_VORBIS.to_vec();
    increment_declared_len(&mut oversized_data, DATA_LEN_OFFSET);

    let cases = [
        ("header", SHORT_VORBIS[..HEADER_LEN - 1].to_vec()),
        (
            "sample header directory",
            SHORT_VORBIS[..sample_headers_end - 1].to_vec(),
        ),
        ("sample payload", SHORT_VORBIS[..data_end - 1].to_vec()),
        ("declared sample header length", oversized_sample_headers),
        ("declared sample payload length", oversized_data),
    ];

    let clip = AudioClip::new("invalid-fsb5".into(), AudioCompressionFormat::Vorbis);
    for (case, bytes) in cases {
        let Err(error) = prepare_standard_source(&clip, bytes) else {
            panic!("{case} must fail strict FSB5 preparation");
        };
        assert!(
            matches!(&error, AudioSourceError::InvalidData(_)),
            "{case} produced an unrelated error: {error}"
        );
    }
}

fn rebuilt_playable_ogg() -> Vec<u8> {
    let clip = AudioClip::new("short".into(), AudioCompressionFormat::Vorbis);
    let prepared = prepare_standard_source(&clip, SHORT_VORBIS.to_vec()).unwrap();
    let mut output = Vec::new();
    prepared.write_to(&mut output).unwrap();
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

struct RejectingSeekWriter;

impl Write for RejectingSeekWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("fixture output failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for RejectingSeekWriter {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Ok(0)
    }
}

struct ThrottledWriter {
    bytes: Vec<u8>,
    max_write: usize,
    fail_after: Option<usize>,
    write_calls: usize,
}

impl ThrottledWriter {
    fn new(max_write: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_write,
            fail_after: None,
            write_calls: 0,
        }
    }

    fn failing(max_write: usize, fail_after: usize) -> Self {
        Self {
            fail_after: Some(fail_after),
            ..Self::new(max_write)
        }
    }
}

impl Write for ThrottledWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.write_calls += 1;
        std::thread::yield_now();

        let remaining = self
            .fail_after
            .map_or(usize::MAX, |limit| limit.saturating_sub(self.bytes.len()));
        if remaining == 0 {
            return Err(io::Error::other("fixture output capacity exhausted"));
        }

        let written = bytes.len().min(self.max_write).min(remaining);
        self.bytes.extend_from_slice(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn texture_writers_encode_caller_owned_bytes() -> Result<(), Box<dyn std::error::Error>> {
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

    let mut jpeg_sink = Cursor::new(Vec::new());
    TextureExporter::write_jpeg(&image, &mut jpeg_sink, 90)?;
    let jpeg_bytes = jpeg_sink.into_inner();
    assert!(jpeg_bytes.starts_with(&[0xff, 0xd8]));
    assert_eq!(
        image::load_from_memory_with_format(&jpeg_bytes, ImageFormat::Jpeg)?.dimensions(),
        image.dimensions()
    );

    let mut bmp_sink = Cursor::new(Vec::new());
    TextureExporter::write_bmp(&image, &mut bmp_sink)?;
    let bmp_bytes = bmp_sink.into_inner();
    assert!(bmp_bytes.starts_with(b"BM"));
    assert_eq!(
        image::load_from_memory_with_format(&bmp_bytes, ImageFormat::Bmp)?.dimensions(),
        image.dimensions()
    );

    let mut tiff_sink = Cursor::new(Vec::new());
    TextureExporter::write_tiff(&image, &mut tiff_sink)?;
    let tiff_bytes = tiff_sink.into_inner();
    assert!(tiff_bytes.starts_with(b"II") || tiff_bytes.starts_with(b"MM"));
    assert_eq!(
        image::load_from_memory_with_format(&tiff_bytes, ImageFormat::Tiff)?.dimensions(),
        image.dimensions()
    );

    Ok(())
}

#[test]
fn texture_writers_reject_invalid_dimensions_before_writing() {
    let empty = RgbaImage::new(0, 0);

    for write in [
        TextureExporter::write_png::<Vec<u8>>,
        TextureExporter::write_bmp::<Vec<u8>>,
    ] {
        let mut sink = Vec::new();
        assert!(matches!(
            write(&empty, &mut sink),
            Err(BinaryError::InvalidData(_))
        ));
        assert!(sink.is_empty());
    }

    let mut jpeg_sink = Vec::new();
    assert!(matches!(
        TextureExporter::write_jpeg(&empty, &mut jpeg_sink, 90),
        Err(BinaryError::InvalidData(_))
    ));
    assert!(jpeg_sink.is_empty());

    let mut tiff_sink = Cursor::new(Vec::new());
    assert!(matches!(
        TextureExporter::write_tiff(&empty, &mut tiff_sink),
        Err(BinaryError::InvalidData(_))
    ));
    assert!(tiff_sink.into_inner().is_empty());

    let too_wide = RgbaImage::new(u32::from(u16::MAX) + 1, 1);
    let mut sink = Vec::new();
    assert!(matches!(
        TextureExporter::write_jpeg(&too_wide, &mut sink, 90),
        Err(BinaryError::InvalidData(_))
    ));
    assert!(sink.is_empty());
}

#[test]
fn texture_writers_preserve_caller_sink_errors() {
    let image = RgbaImage::new(1, 1);

    let error = TextureExporter::write_jpeg(&image, &mut RejectingWriter, 90).unwrap_err();
    assert!(matches!(error, BinaryError::Io(source) if source.kind() == io::ErrorKind::Other));

    let error = TextureExporter::write_bmp(&image, &mut RejectingWriter).unwrap_err();
    assert!(matches!(error, BinaryError::Io(source) if source.kind() == io::ErrorKind::Other));

    let error = TextureExporter::write_tiff(&image, &mut RejectingSeekWriter).unwrap_err();
    assert!(matches!(error, BinaryError::Io(source) if source.kind() == io::ErrorKind::Other));
}

#[test]
fn sprite_png_writer_encodes_caller_owned_bytes() -> Result<(), Box<dyn std::error::Error>> {
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
    let processor = SpriteProcessor::new();

    let mut png_sink = Cursor::new(Vec::new());
    processor.write_sprite_png(&sprite, &texture, &mut png_sink)?;
    let png_bytes = png_sink.into_inner();

    assert_eq!(&png_bytes[..PNG_SIGNATURE.len()], PNG_SIGNATURE);
    let decoded = image::load_from_memory_with_format(&png_bytes, ImageFormat::Png)?.to_rgba8();
    assert_eq!(decoded.dimensions(), (2, 1));
    assert_eq!(decoded.as_raw(), &[0, 0, 255, 128, 255, 255, 255, 64]);
    Ok(())
}

#[test]
fn codecs_bound_write_calls_and_propagate_partial_sink_failures() {
    let samples = (0..16_384)
        .map(|index| (index % 257) as f32 / 128.0 - 1.0)
        .collect();
    let audio = DecodedAudio::new(samples, 48_000, 2);

    let mut wav_sink = ThrottledWriter::new(257);
    AudioExporter::write_wav(&audio, &mut wav_sink).unwrap();
    assert_eq!(wav_sink.bytes.len(), 44 + audio.samples.len() * 2);
    assert_eq!(&wav_sink.bytes[..4], b"RIFF");
    assert!(
        wav_sink.write_calls > 100,
        "fixture must exercise partial writes"
    );

    let mut pcm_sink = ThrottledWriter::new(usize::MAX);
    AudioExporter::write_raw_pcm(&audio, &mut pcm_sink, 16).unwrap();
    assert_eq!(pcm_sink.bytes.len(), audio.samples.len() * 2);
    assert!(
        pcm_sink.write_calls <= 16,
        "PCM encoding must batch samples instead of writing each one"
    );

    let image = RgbaImage::from_fn(128, 128, |x, y| {
        image::Rgba([
            x.wrapping_mul(17) as u8,
            y.wrapping_mul(29) as u8,
            x.wrapping_mul(y) as u8,
            255,
        ])
    });
    let mut png_sink = ThrottledWriter::new(257);
    TextureExporter::write_png(&image, &mut png_sink).unwrap();
    assert!(png_sink.bytes.starts_with(PNG_SIGNATURE));
    assert!(
        png_sink.write_calls > 1,
        "fixture must exercise partial writes"
    );

    let mut failing_audio = ThrottledWriter::failing(257, 1_024);
    let error = AudioExporter::write_wav(&audio, &mut failing_audio).unwrap_err();
    assert!(matches!(error, BinaryError::Io(source) if source.kind() == io::ErrorKind::Other));

    let mut failing_texture = ThrottledWriter::failing(31, 64);
    let error = TextureExporter::write_png(&image, &mut failing_texture).unwrap_err();
    assert!(matches!(error, BinaryError::Io(source) if source.kind() == io::ErrorKind::Other));
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
