//! Audio artifact encoding utilities
//!
//! Every encoder writes to storage owned and managed by the caller.

use super::formats::AudioCompressionFormat;
use super::fsb5::{self, Fsb5Error, PreparedVorbisOgg};
use super::ogg::is_ogg_vorbis;
use super::types::{AudioClip, DecodedAudio};
use crate::error::{BinaryError, Result};
use std::io::{self, Write};
use std::mem::size_of;
use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError, DigestV1};

/// A validated standard audio artifact tied to the exact source bytes used at
/// preparation time.
pub struct PreparedAudioSource {
    source_length: u64,
    source_digest: DigestV1,
    encoding: PreparedAudioEncoding,
}

enum PreparedAudioEncoding {
    Passthrough,
    Fsb5Vorbis(PreparedVorbisOgg),
}

impl PreparedAudioSource {
    /// Write the prepared artifact without reparsing or allocating from
    /// attacker-controlled source metadata.
    pub fn write_to<W: Write + ?Sized>(
        &self,
        bytes: &[u8],
        writer: &mut W,
    ) -> std::result::Result<(), AudioSourceError> {
        let source_length = u64::try_from(bytes.len())
            .map_err(|_| AudioSourceError::InvalidData("audio source length exceeds u64".into()))?;
        if source_length != self.source_length || DigestV1::hash_bytes(bytes) != self.source_digest
        {
            return Err(AudioSourceError::SourceChanged);
        }
        match &self.encoding {
            PreparedAudioEncoding::Passthrough => {
                writer.write_all(bytes).map_err(AudioSourceError::Output)
            }
            PreparedAudioEncoding::Fsb5Vorbis(prepared) => prepared
                .write_to(bytes, writer)
                .map_err(AudioSourceError::from_fsb5),
        }
    }
}

/// Failures while validating or writing a playable audio artifact.
#[derive(Debug, Error)]
pub enum AudioSourceError {
    #[error("invalid audio source data: {0}")]
    InvalidData(String),
    #[error("audio compression format {0:?} has no validated standard-container export")]
    UnsupportedFormat(AudioCompressionFormat),
    #[error("audio source bytes changed after preparation")]
    SourceChanged,
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to write audio output: {0}")]
    Output(#[source] io::Error),
}

impl AudioSourceError {
    fn from_fsb5(error: Fsb5Error) -> Self {
        match error {
            Fsb5Error::Budget(error) => Self::Budget(error),
            Fsb5Error::Output(error) => Self::Output(error),
            error => Self::InvalidData(error.to_string()),
        }
    }
}

/// Audio exporter utility
///
/// This struct provides methods for exporting decoded audio data to various formats.
pub struct AudioExporter;

impl AudioExporter {
    /// Validate a Unity audio payload and prepare a deterministic standard
    /// container export under the caller's load budget.
    pub fn prepare_standard_source(
        clip: &AudioClip,
        bytes: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<PreparedAudioSource, AudioSourceError> {
        if bytes.is_empty() {
            return Err(AudioSourceError::InvalidData(
                "audio source data is empty".into(),
            ));
        }
        let source_length = u64::try_from(bytes.len())
            .map_err(|_| AudioSourceError::InvalidData("audio source length exceeds u64".into()))?;
        budget.consume_bytes(source_length)?;

        let encoding = match clip.compression_format() {
            AudioCompressionFormat::Vorbis if is_ogg_vorbis(bytes) => {
                PreparedAudioEncoding::Passthrough
            }
            AudioCompressionFormat::Vorbis if fsb5::is_fsb5(bytes) => {
                let subsound_index = usize::try_from(clip.subsound_index()).map_err(|_| {
                    AudioSourceError::InvalidData(
                        "AudioClip subsound index cannot be negative".into(),
                    )
                })?;
                PreparedAudioEncoding::Fsb5Vorbis(
                    PreparedVorbisOgg::prepare(bytes, subsound_index, budget)
                        .map_err(AudioSourceError::from_fsb5)?,
                )
            }
            AudioCompressionFormat::Vorbis => {
                return Err(AudioSourceError::InvalidData(
                    "Vorbis AudioClip data is neither an Ogg stream nor an FSB5 bank".into(),
                ));
            }
            format @ (AudioCompressionFormat::PCM | AudioCompressionFormat::ADPCM)
                if is_wave(bytes, format) =>
            {
                PreparedAudioEncoding::Passthrough
            }
            AudioCompressionFormat::MP3 if is_mp3(bytes) => PreparedAudioEncoding::Passthrough,
            AudioCompressionFormat::AAC if is_adts_aac(bytes) => PreparedAudioEncoding::Passthrough,
            format => return Err(AudioSourceError::UnsupportedFormat(format)),
        };

        Ok(PreparedAudioSource {
            source_length,
            source_digest: DigestV1::hash_bytes(bytes),
            encoding,
        })
    }

    /// Write a standard playable container from an AudioClip payload.
    ///
    /// Vorbis clips may carry an FSB5 bank instead of an Ogg stream. In that
    /// case this method reconstructs the selected subsound as Ogg/Vorbis. It
    /// rejects unknown Vorbis payloads rather than emitting FSB bytes with an
    /// `.ogg` extension.
    pub fn write_standard_source<W: Write + ?Sized>(
        clip: &AudioClip,
        bytes: &[u8],
        writer: &mut W,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<(), AudioSourceError> {
        Self::prepare_standard_source(clip, bytes, budget)?.write_to(bytes, writer)
    }

    /// Write WAV bytes to a caller-owned sink.
    ///
    /// The sink is not flushed. Callers that buffer output retain control over
    /// the flush and durability policy.
    pub fn write_wav<W: Write + ?Sized>(audio: &DecodedAudio, writer: &mut W) -> Result<()> {
        if audio.sample_rate == 0 || audio.channels == 0 {
            return Err(BinaryError::invalid_data(
                "WAV sample rate and channel count must be non-zero",
            ));
        }
        let channels = u16::try_from(audio.channels)
            .map_err(|_| BinaryError::invalid_data("WAV channel count exceeds u16"))?;
        if !audio.samples.len().is_multiple_of(usize::from(channels)) {
            return Err(BinaryError::invalid_data(
                "WAV sample count must contain complete channel frames",
            ));
        }
        let block_align = channels
            .checked_mul(2)
            .ok_or_else(|| BinaryError::invalid_data("WAV block alignment overflow"))?;
        let byte_rate = audio
            .sample_rate
            .checked_mul(u32::from(block_align))
            .ok_or_else(|| BinaryError::invalid_data("WAV byte rate overflow"))?;
        let data_size = audio
            .samples
            .len()
            .checked_mul(size_of::<i16>())
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| BinaryError::invalid_data("WAV sample data exceeds RIFF limits"))?;
        let file_size = 36_u32
            .checked_add(data_size)
            .ok_or_else(|| BinaryError::invalid_data("WAV file size exceeds RIFF limits"))?;

        // Write WAV header
        writer.write_all(b"RIFF")?;
        writer.write_all(&file_size.to_le_bytes())?;
        writer.write_all(b"WAVE")?;

        // Write format chunk
        writer.write_all(b"fmt ")?;
        writer.write_all(&16u32.to_le_bytes())?; // Chunk size
        writer.write_all(&1u16.to_le_bytes())?; // Audio format (PCM)
        writer.write_all(&channels.to_le_bytes())?;
        writer.write_all(&audio.sample_rate.to_le_bytes())?;
        writer.write_all(&byte_rate.to_le_bytes())?;
        writer.write_all(&block_align.to_le_bytes())?;
        writer.write_all(&16u16.to_le_bytes())?; // Bits per sample

        // Write data chunk
        writer.write_all(b"data")?;
        writer.write_all(&data_size.to_le_bytes())?;

        // Write sample data
        write_i16_samples(&audio.samples, writer)?;

        Ok(())
    }

    /// Write raw little-endian PCM bytes to a caller-owned sink.
    ///
    /// Supported bit depths are 16 and 32. The sink is not flushed.
    pub fn write_raw_pcm<W: Write + ?Sized>(
        audio: &DecodedAudio,
        writer: &mut W,
        bit_depth: u8,
    ) -> Result<()> {
        match bit_depth {
            16 => {
                write_i16_samples(&audio.samples, writer)?;
            }
            32 => {
                write_i32_samples(&audio.samples, writer)?;
            }
            _ => {
                return Err(BinaryError::invalid_data(
                    "Unsupported bit depth for PCM export",
                ));
            }
        }

        Ok(())
    }
}

const PCM_CHUNK_SAMPLES: usize = 1_024;

fn write_i16_samples<W: Write + ?Sized>(samples: &[f32], writer: &mut W) -> Result<()> {
    let mut bytes = [0_u8; PCM_CHUNK_SAMPLES * size_of::<i16>()];
    for samples in samples.chunks(PCM_CHUNK_SAMPLES) {
        for (sample, output) in samples.iter().zip(bytes.chunks_exact_mut(size_of::<i16>())) {
            let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            output.copy_from_slice(&sample.to_le_bytes());
        }
        writer.write_all(&bytes[..samples.len() * size_of::<i16>()])?;
    }
    Ok(())
}

fn write_i32_samples<W: Write + ?Sized>(samples: &[f32], writer: &mut W) -> Result<()> {
    let mut bytes = [0_u8; PCM_CHUNK_SAMPLES * size_of::<i32>()];
    for samples in samples.chunks(PCM_CHUNK_SAMPLES) {
        for (sample, output) in samples.iter().zip(bytes.chunks_exact_mut(size_of::<i32>())) {
            let sample = (sample.clamp(-1.0, 1.0) * i32::MAX as f32) as i32;
            output.copy_from_slice(&sample.to_le_bytes());
        }
        writer.write_all(&bytes[..samples.len() * size_of::<i32>()])?;
    }
    Ok(())
}

fn is_wave(bytes: &[u8], compression: AudioCompressionFormat) -> bool {
    if bytes.len() < 12 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return false;
    }
    let Some(declared) = read_u32_le(bytes, 4) else {
        return false;
    };
    if usize::try_from(declared)
        .ok()
        .and_then(|length| length.checked_add(8))
        != Some(bytes.len())
    {
        return false;
    }

    let mut cursor = 12_usize;
    let mut block_align = None;
    let mut data_size = None;
    while cursor < bytes.len() {
        let Some(header_end) = cursor.checked_add(8) else {
            return false;
        };
        if header_end > bytes.len() {
            return false;
        }
        let Some(chunk_size) =
            read_u32_le(bytes, cursor + 4).and_then(|size| usize::try_from(size).ok())
        else {
            return false;
        };
        let Some(chunk_end) = header_end.checked_add(chunk_size) else {
            return false;
        };
        if chunk_end > bytes.len() {
            return false;
        }
        match &bytes[cursor..cursor + 4] {
            b"fmt " => {
                if block_align.is_some() || chunk_size < 16 {
                    return false;
                }
                let Some(format_tag) = read_u16_le(bytes, header_end) else {
                    return false;
                };
                let Some(channels) = read_u16_le(bytes, header_end + 2) else {
                    return false;
                };
                let Some(sample_rate) = read_u32_le(bytes, header_end + 4) else {
                    return false;
                };
                let Some(byte_rate) = read_u32_le(bytes, header_end + 8) else {
                    return false;
                };
                let Some(actual_block_align) = read_u16_le(bytes, header_end + 12) else {
                    return false;
                };
                let Some(bits_per_sample) = read_u16_le(bytes, header_end + 14) else {
                    return false;
                };
                if !valid_wave_format(
                    compression,
                    format_tag,
                    channels,
                    sample_rate,
                    byte_rate,
                    actual_block_align,
                    bits_per_sample,
                    &bytes[header_end..chunk_end],
                ) {
                    return false;
                }
                block_align = Some(actual_block_align);
            }
            b"data" if data_size.is_some() => return false,
            b"data" => data_size = Some(chunk_size),
            _ => {}
        }
        let Some(next) = chunk_end.checked_add(chunk_size & 1) else {
            return false;
        };
        if next > bytes.len() {
            return false;
        }
        cursor = next;
    }

    matches!(
        (block_align, data_size),
        (Some(block_align), Some(data_size))
            if data_size > 0 && data_size.is_multiple_of(usize::from(block_align))
    )
}

fn is_mp3(bytes: &[u8]) -> bool {
    let Some(mut cursor) = id3v2_end(bytes) else {
        return false;
    };
    let frame_end = if bytes.len().saturating_sub(cursor) >= 128
        && bytes[bytes.len() - 128..].starts_with(b"TAG")
    {
        bytes.len() - 128
    } else {
        bytes.len()
    };
    let mut frames = 0_usize;
    while cursor < frame_end {
        let Some(frame_length) = mp3_frame_length(&bytes[cursor..frame_end]) else {
            return false;
        };
        let Some(next) = cursor.checked_add(frame_length) else {
            return false;
        };
        if next > frame_end {
            return false;
        }
        cursor = next;
        frames = frames.saturating_add(1);
    }
    frames > 0 && cursor == frame_end
}

fn is_adts_aac(bytes: &[u8]) -> bool {
    let mut cursor = 0_usize;
    let mut frames = 0_usize;
    while cursor < bytes.len() {
        let Some(header) = bytes.get(cursor..cursor.saturating_add(7)) else {
            return false;
        };
        let sampling_frequency_index = (header[2] >> 2) & 0x0F;
        if header[0] != 0xFF
            || header[1] & 0xF0 != 0xF0
            || header[1] & 0x06 != 0
            || sampling_frequency_index >= 13
        {
            return false;
        }
        let channel_configuration = (u16::from(header[2] & 0x01) << 2) | u16::from(header[3] >> 6);
        if channel_configuration == 0 {
            return false;
        }
        let header_length = if header[1] & 0x01 == 0 { 9 } else { 7 };
        let frame_length = (usize::from(header[3] & 0x03) << 11)
            | (usize::from(header[4]) << 3)
            | usize::from(header[5] >> 5);
        if frame_length <= header_length {
            return false;
        }
        let Some(next) = cursor.checked_add(frame_length) else {
            return false;
        };
        if next > bytes.len() {
            return false;
        }
        cursor = next;
        frames = frames.saturating_add(1);
    }
    frames > 0
}

fn valid_wave_format(
    compression: AudioCompressionFormat,
    format_tag: u16,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
    format_chunk: &[u8],
) -> bool {
    if channels == 0
        || sample_rate == 0
        || byte_rate == 0
        || block_align == 0
        || bits_per_sample == 0
    {
        return false;
    }
    match compression {
        AudioCompressionFormat::PCM => {
            let Some(bytes_per_sample) = bits_per_sample
                .is_multiple_of(8)
                .then_some(bits_per_sample / 8)
                .filter(|value| *value != 0)
            else {
                return false;
            };
            let Some(expected_block_align) = channels.checked_mul(bytes_per_sample) else {
                return false;
            };
            format_tag == 1
                && block_align == expected_block_align
                && sample_rate.checked_mul(u32::from(block_align)) == Some(byte_rate)
        }
        AudioCompressionFormat::ADPCM => match format_tag {
            0x11 => valid_ima_adpcm_format(
                format_chunk,
                channels,
                sample_rate,
                byte_rate,
                block_align,
                bits_per_sample,
            ),
            2 => valid_microsoft_adpcm_format(
                format_chunk,
                channels,
                sample_rate,
                byte_rate,
                block_align,
                bits_per_sample,
            ),
            _ => false,
        },
        _ => false,
    }
}

fn valid_ima_adpcm_format(
    format: &[u8],
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
) -> bool {
    let Some(extension_size) = read_u16_le(format, 16) else {
        return false;
    };
    let Some(samples_per_block) = read_u16_le(format, 18) else {
        return false;
    };
    if extension_size != 2 || format.len() != 20 || bits_per_sample != 4 {
        return false;
    }
    let Some(expected_samples) =
        adpcm_samples_per_block(channels, block_align, bits_per_sample, 4, 1)
    else {
        return false;
    };
    samples_per_block == expected_samples
        && adpcm_byte_rate(sample_rate, block_align, samples_per_block) == Some(byte_rate)
}

fn valid_microsoft_adpcm_format(
    format: &[u8],
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
) -> bool {
    let Some(extension_size) = read_u16_le(format, 16) else {
        return false;
    };
    let Some(samples_per_block) = read_u16_le(format, 18) else {
        return false;
    };
    let Some(coefficient_count) = read_u16_le(format, 20) else {
        return false;
    };
    let Some(coefficient_bytes) = usize::from(coefficient_count).checked_mul(4) else {
        return false;
    };
    let Some(expected_extension_size) = coefficient_bytes.checked_add(4) else {
        return false;
    };
    let Some(expected_format_size) = expected_extension_size.checked_add(18) else {
        return false;
    };
    if coefficient_count == 0
        || usize::from(extension_size) != expected_extension_size
        || format.len() != expected_format_size
        || bits_per_sample != 4
    {
        return false;
    }
    let Some(expected_samples) =
        adpcm_samples_per_block(channels, block_align, bits_per_sample, 7, 2)
    else {
        return false;
    };
    samples_per_block == expected_samples
        && adpcm_byte_rate(sample_rate, block_align, samples_per_block) == Some(byte_rate)
}

fn adpcm_samples_per_block(
    channels: u16,
    block_align: u16,
    bits_per_sample: u16,
    header_bytes_per_channel: u16,
    initial_samples: u32,
) -> Option<u16> {
    let header_bytes = channels.checked_mul(header_bytes_per_channel)?;
    let encoded_bytes = block_align.checked_sub(header_bytes)?;
    let numerator = u32::from(encoded_bytes) * 8;
    let denominator = u32::from(bits_per_sample) * u32::from(channels);
    if denominator == 0 || !numerator.is_multiple_of(denominator) {
        return None;
    }
    numerator
        .checked_div(denominator)?
        .checked_add(initial_samples)
        .and_then(|samples| u16::try_from(samples).ok())
}

fn adpcm_byte_rate(sample_rate: u32, block_align: u16, samples_per_block: u16) -> Option<u32> {
    sample_rate
        .checked_mul(u32::from(block_align))?
        .checked_div(u32::from(samples_per_block))
}

fn id3v2_end(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(b"ID3") {
        return Some(0);
    }
    let header = bytes.get(..10)?;
    let version = header[3];
    if !(2..=4).contains(&version) || header[4] == 0xFF {
        return None;
    }
    let flags = header[5];
    let reserved_flags = match version {
        2 => 0x3F,
        3 => 0x1F,
        4 => 0x0F,
        _ => return None,
    };
    if flags & reserved_flags != 0 || header[6..10].iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    let payload_length = header[6..10]
        .iter()
        .fold(0_usize, |length, byte| (length << 7) | usize::from(*byte));
    let footer_length = usize::from(version == 4 && flags & 0x10 != 0) * 10;
    10_usize
        .checked_add(payload_length)?
        .checked_add(footer_length)
        .filter(|end| *end <= bytes.len())
}

fn mp3_frame_length(bytes: &[u8]) -> Option<usize> {
    let header = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
    if header & 0xFFE0_0000 != 0xFFE0_0000 {
        return None;
    }
    let version = (header >> 19) & 0x03;
    let layer = (header >> 17) & 0x03;
    let bitrate_index = usize::try_from((header >> 12) & 0x0F).ok()?;
    let sample_rate_index = usize::try_from((header >> 10) & 0x03).ok()?;
    if version == 1
        || layer != 1
        || bitrate_index == 0
        || bitrate_index == 15
        || sample_rate_index == 3
        || header & 0x03 == 2
    {
        return None;
    }
    const MPEG1_LAYER3_KBPS: [usize; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    const MPEG2_LAYER3_KBPS: [usize; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    const SAMPLE_RATES: [usize; 3] = [44_100, 48_000, 32_000];
    let (bitrate, coefficient, divisor) = match version {
        3 => (MPEG1_LAYER3_KBPS[bitrate_index], 144_000_usize, 1),
        2 => (MPEG2_LAYER3_KBPS[bitrate_index], 72_000_usize, 2),
        0 => (MPEG2_LAYER3_KBPS[bitrate_index], 72_000_usize, 4),
        _ => return None,
    };
    let sample_rate = SAMPLE_RATES[sample_rate_index] / divisor;
    coefficient
        .checked_mul(bitrate)?
        .checked_div(sample_rate)?
        .checked_add(usize::from(header >> 9 & 1 != 0))
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
