#![cfg(feature = "audio")]

use indexmap::IndexMap;
use unity_asset_binary::{
    asset::{ObjectInfo, class_ids},
    object::UnityObject,
};
use unity_asset_core::{AssetLoadBudget, UnityClass, UnityValue};
use unity_asset_decode::audio::{
    AudioClipLayout, AudioCompressionFormat, AudioSourceError, PreparedAudioSource,
};
const SHORT_VORBIS: &[u8] = include_bytes!("fixtures/short_vorbis.fsb");

fn prepare_standard_source(
    format: AudioCompressionFormat,
    bytes: Vec<u8>,
) -> Result<PreparedAudioSource, AudioSourceError> {
    let class = UnityClass::with_properties(
        class_ids::AUDIO_CLIP,
        "AudioClip".to_owned(),
        "1".to_owned(),
        IndexMap::from([
            ("m_Name".to_owned(), UnityValue::String("Clip".to_owned())),
            (
                "m_CompressionFormat".to_owned(),
                UnityValue::Integer(format as i64),
            ),
            ("m_SubsoundIndex".to_owned(), UnityValue::Integer(0)),
            ("m_AudioData".to_owned(), UnityValue::Bytes(bytes)),
        ]),
    );
    let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::AUDIO_CLIP).unwrap();
    let object = UnityObject::from_info_and_class(info, class);
    let layout = AudioClipLayout::inspect(&object).unwrap();
    let mut budget = AssetLoadBudget::default();
    let source = layout
        .payload()
        .embedded()
        .expect("test AudioClip must select its embedded payload")
        .materialize("test audio source", &mut budget)
        .expect("default test budget must materialize the selected payload");
    PreparedAudioSource::prepare(layout, source, &mut budget)
}

fn adts_frame(payload: &[u8], sampling_frequency_index: u8) -> Vec<u8> {
    assert!(sampling_frequency_index <= 0x0f);
    let frame_length = 7_usize.checked_add(payload.len()).unwrap();
    assert!(frame_length <= 0x1fff);
    let channel_configuration = 2_u8;
    let mut bytes = Vec::with_capacity(frame_length);
    bytes.extend_from_slice(&[
        0xFF,
        0xF1,
        0x40 | (sampling_frequency_index << 2) | (channel_configuration >> 2),
        ((channel_configuration & 0x03) << 6) | u8::try_from((frame_length >> 11) & 0x03).unwrap(),
        u8::try_from((frame_length >> 3) & 0xFF).unwrap(),
        u8::try_from((frame_length & 0x07) << 5).unwrap() | 0x1F,
        0xFC,
    ]);
    bytes.extend_from_slice(payload);
    bytes
}

fn minimal_m4a() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&24_u32.to_be_bytes());
    bytes.extend_from_slice(b"ftypM4A \0\0\0\0M4A isom");
    bytes
}

#[test]
fn standard_audio_source_rejects_headerless_pcm_and_adpcm() {
    for format in [AudioCompressionFormat::PCM, AudioCompressionFormat::ADPCM] {
        let error = prepare_standard_source(format, vec![1, 2, 3, 4])
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
        let Err(error) = prepare_standard_source(format, bytes.to_vec()) else {
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
        let bytes = adts_frame(
            &[0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C],
            sampling_frequency_index,
        );
        let result = prepare_standard_source(AudioCompressionFormat::AAC, bytes);

        assert!(matches!(result, Err(AudioSourceError::InvalidData(_))));
    }
}

#[test]
fn aac_passthrough_requires_complete_consistent_adts_framing() {
    let payload = [0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C];

    let mut complete = adts_frame(&payload, 4);
    complete.extend_from_slice(&adts_frame(&payload, 4));
    let expected = complete.clone();
    let prepared = prepare_standard_source(AudioCompressionFormat::AAC, complete).unwrap();
    let mut output = Vec::new();
    prepared.write_to(&mut output).unwrap();
    assert_eq!(output, expected);

    let cases = [
        adts_frame(&[0], 4),
        {
            let mut truncated = adts_frame(&payload, 4);
            truncated.pop();
            truncated
        },
        {
            let mut multiple_raw_blocks = adts_frame(&payload, 4);
            multiple_raw_blocks[6] |= 1;
            multiple_raw_blocks
        },
        {
            let mut crc_protected = adts_frame(&payload, 4);
            crc_protected[1] &= !1;
            crc_protected
        },
        {
            let mut inconsistent = adts_frame(&payload, 4);
            inconsistent.extend_from_slice(&adts_frame(&payload, 3));
            inconsistent
        },
    ];

    for bytes in cases {
        assert!(matches!(
            prepare_standard_source(AudioCompressionFormat::AAC, bytes),
            Err(AudioSourceError::InvalidData(_))
        ));
    }
}

#[test]
fn m4a_aac_is_an_unsupported_container_not_corrupt_aac() {
    let result = prepare_standard_source(AudioCompressionFormat::AAC, minimal_m4a());

    assert!(matches!(
        result,
        Err(AudioSourceError::UnsupportedContainer {
            format: AudioCompressionFormat::AAC,
            container: "ISO BMFF/M4A AAC",
        })
    ));
}

#[test]
fn standard_audio_source_rejects_adpcm_without_codec_extension() {
    let bytes = wave_fixture(0x11, 1, 8_000, 8_000, 4, 4, &[], &[0; 4]);
    let result = prepare_standard_source(AudioCompressionFormat::ADPCM, bytes.to_vec());

    assert!(matches!(result, Err(AudioSourceError::InvalidData(_))));
}

#[test]
fn standard_audio_source_accepts_complete_minimal_containers() {
    let wav = wave_fixture(1, 1, 8_000, 16_000, 2, 16, &[], &[0, 0]);

    let mut mp3 = vec![0_u8; 417];
    mp3[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);

    // AAC passthrough guarantees complete, consistent ADTS framing. It does
    // not claim to validate the opaque raw AAC syntax without a decoder.
    let aac = adts_frame(&[0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C], 4);
    let ima_adpcm = wave_fixture(0x11, 1, 8_000, 7_111, 8, 4, &[2, 0, 9, 0], &[0; 8]);

    for (format, bytes) in [
        (AudioCompressionFormat::PCM, wav),
        (AudioCompressionFormat::ADPCM, ima_adpcm),
        (AudioCompressionFormat::MP3, mp3),
        (AudioCompressionFormat::AAC, aac),
    ] {
        let expected = bytes.clone();
        let prepared = prepare_standard_source(format, bytes).unwrap();
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
    let source = rebuilt_playable_ogg();
    let Err(error) = prepare_standard_source(AudioCompressionFormat::Vorbis, source.clone()) else {
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
    let Err(error) = prepare_standard_source(AudioCompressionFormat::Vorbis, corrupt) else {
        panic!("a corrupt Ogg checksum must not be published as a direct audio source");
    };
    assert!(matches!(error, AudioSourceError::InvalidData(_)));
}

#[test]
fn standard_audio_source_rejects_crc_correct_non_vorbis_ogg() {
    let source = crc_correct_non_vorbis_ogg();
    let Err(error) = prepare_standard_source(AudioCompressionFormat::Vorbis, source) else {
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

    for (case, bytes) in cases {
        let Err(error) = prepare_standard_source(AudioCompressionFormat::Vorbis, bytes) else {
            panic!("{case} must fail strict FSB5 preparation");
        };
        assert!(
            matches!(&error, AudioSourceError::InvalidData(_)),
            "{case} produced an unrelated error: {error}"
        );
    }
}

fn rebuilt_playable_ogg() -> Vec<u8> {
    let prepared =
        prepare_standard_source(AudioCompressionFormat::Vorbis, SHORT_VORBIS.to_vec()).unwrap();
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
