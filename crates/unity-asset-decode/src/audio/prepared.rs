//! Audio artifact encoding utilities
//!
//! Every encoder writes to storage owned and managed by the caller.

use super::formats::AudioCompressionFormat;
use super::fsb5::{self, Fsb5Error, PreparedVorbisOgg};
use super::inspection::AudioClipLayout;
use super::ogg::final_ogg_granule;
use crate::descriptor::{
    MediaDescriptor, MediaDescriptorError, MediaOutputEstimate, PreparedAudioSourceKind,
};
use crate::media::BudgetedMediaBytes;
use std::collections::TryReserveError;
use std::io::{self, Write};
use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError};

/// A standard audio artifact with strictly validated source semantics and exact owned output bytes.
pub struct PreparedAudioSource {
    output: PreparedAudioOutput,
    descriptor: MediaDescriptor,
}

enum PreparedAudioOutput {
    Source(BudgetedMediaBytes),
    RebuiltOgg(PreparedVorbisOgg),
}

impl PreparedAudioOutput {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Source(source) => source.as_bytes(),
            Self::RebuiltOgg(output) => output.as_bytes(),
        }
    }
}

impl PreparedAudioSource {
    /// Whether this build has a strict standard-container path for the encoding.
    #[must_use]
    pub const fn supports(compression_format: AudioCompressionFormat) -> bool {
        matches!(
            compression_format,
            AudioCompressionFormat::PCM
                | AudioCompressionFormat::Vorbis
                | AudioCompressionFormat::ADPCM
        )
    }

    /// Prepares one strictly inspected AudioClip and its already-resolved payload.
    pub fn prepare(
        layout: AudioClipLayout<'_>,
        bytes: BudgetedMediaBytes,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Self, AudioSourceError> {
        Self::prepare_source(
            layout.compression_format(),
            layout.subsound_index(),
            bytes,
            budget,
        )
    }

    #[must_use]
    pub const fn descriptor(&self) -> &MediaDescriptor {
        &self.descriptor
    }

    /// Write the prepared artifact without reparsing or allocating from
    /// attacker-controlled source metadata.
    pub fn write_to<W: Write + ?Sized>(
        &self,
        writer: &mut W,
    ) -> std::result::Result<(), AudioSourceError> {
        writer
            .write_all(self.output.as_bytes())
            .map_err(AudioSourceError::Output)
    }
}

/// Failures while validating or writing a standard-container audio artifact.
#[derive(Debug, Error)]
pub enum AudioSourceError {
    #[error("invalid audio source data: {0}")]
    InvalidData(String),
    #[error("audio compression format {0:?} has no validated standard-container export")]
    UnsupportedFormat(AudioCompressionFormat),
    #[error(
        "{format:?} source container {container} has no caller-budgeted strict validation path"
    )]
    UnsupportedContainer {
        format: AudioCompressionFormat,
        container: &'static str,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Descriptor(#[from] MediaDescriptorError),
    #[error("failed to allocate {resource} ({requested} bytes): {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("failed to write audio output: {0}")]
    Output(#[source] io::Error),
}

impl AudioSourceError {
    fn from_fsb5(error: Fsb5Error) -> Self {
        match error {
            Fsb5Error::Budget(error) => Self::Budget(error),
            Fsb5Error::Output(error) => Self::Output(error),
            Fsb5Error::Allocation {
                resource,
                requested,
                source,
            } => Self::Allocation {
                resource,
                requested,
                source,
            },
            error => Self::InvalidData(error.to_string()),
        }
    }
}

impl PreparedAudioSource {
    fn prepare_source(
        compression_format: AudioCompressionFormat,
        subsound_index: i32,
        bytes: BudgetedMediaBytes,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<PreparedAudioSource, AudioSourceError> {
        bytes.validate_budget(budget)?;
        if !Self::supports(compression_format) {
            return Err(AudioSourceError::UnsupportedFormat(compression_format));
        }
        if bytes.is_empty() {
            return Err(AudioSourceError::InvalidData(
                "audio source data is empty".into(),
            ));
        }
        let source_length = u64::try_from(bytes.len())
            .map_err(|_| AudioSourceError::InvalidData("audio source length exceeds u64".into()))?;
        let source = bytes.as_bytes();

        let (source_kind, rebuilt_ogg) = match compression_format {
            AudioCompressionFormat::Vorbis if source.starts_with(b"OggS") => {
                if final_ogg_granule(source).is_none() {
                    return Err(AudioSourceError::InvalidData(
                        "Ogg source has invalid framing, continuation, sequence, checksum, or EOS"
                            .into(),
                    ));
                }
                return Err(AudioSourceError::UnsupportedContainer {
                    format: AudioCompressionFormat::Vorbis,
                    container: "Ogg Vorbis",
                });
            }
            AudioCompressionFormat::Vorbis if fsb5::is_fsb5(source) => {
                let subsound_index = usize::try_from(subsound_index).map_err(|_| {
                    AudioSourceError::InvalidData(
                        "AudioClip subsound index cannot be negative".into(),
                    )
                })?;
                (
                    PreparedAudioSourceKind::Fsb5Vorbis,
                    Some(
                        PreparedVorbisOgg::prepare(source, subsound_index, budget)
                            .map_err(AudioSourceError::from_fsb5)?,
                    ),
                )
            }
            AudioCompressionFormat::Vorbis => {
                return Err(AudioSourceError::InvalidData(
                    "Vorbis AudioClip data is neither an Ogg stream nor an FSB5 bank".into(),
                ));
            }
            format @ (AudioCompressionFormat::PCM | AudioCompressionFormat::ADPCM)
                if is_wave(source, format) =>
            {
                (
                    if format == AudioCompressionFormat::PCM {
                        PreparedAudioSourceKind::WavePcm
                    } else {
                        PreparedAudioSourceKind::WaveAdpcm
                    },
                    None,
                )
            }
            format if Self::supports(format) => {
                return Err(AudioSourceError::InvalidData(
                    "audio bytes do not match the container declared by the AudioClip encoding"
                        .into(),
                ));
            }
            format => return Err(AudioSourceError::UnsupportedFormat(format)),
        };

        let output_length = match &rebuilt_ogg {
            Some(output) => u64::try_from(output.as_bytes().len()).map_err(|_| {
                AudioSourceError::InvalidData("rebuilt Ogg length exceeds u64".into())
            })?,
            None => source_length,
        };
        let descriptor = MediaDescriptor::audio(
            source_kind,
            source_length,
            MediaOutputEstimate::exact(output_length)?,
        )?;

        let output = rebuilt_ogg.map_or_else(
            || PreparedAudioOutput::Source(bytes),
            PreparedAudioOutput::RebuiltOgg,
        );
        Ok(PreparedAudioSource { output, descriptor })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;

    const SHORT_VORBIS: &[u8] = include_bytes!("../../tests/fixtures/short_vorbis.fsb");

    fn minimal_pcm_wave() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(46);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&38_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    fn prepare_short_vorbis(
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<PreparedAudioSource, AudioSourceError> {
        let source =
            BudgetedMediaBytes::from_vec(SHORT_VORBIS.to_vec(), "test FSB5 source", budget)?;
        PreparedAudioSource::prepare_source(AudioCompressionFormat::Vorbis, 0, source, budget)
    }

    #[test]
    fn audio_preparation_reuses_the_budgeted_source_allocation() {
        let mut budget = AssetLoadBudget::default();
        let source =
            BudgetedMediaBytes::from_vec(minimal_pcm_wave(), "test audio source", &mut budget)
                .unwrap();
        let source_charge = budget.usage().bytes;

        let prepared = PreparedAudioSource::prepare_source(
            AudioCompressionFormat::PCM,
            0,
            source,
            &mut budget,
        )
        .unwrap();

        assert_eq!(budget.usage().bytes, source_charge);
        let mut output = Vec::new();
        prepared.write_to(&mut output).unwrap();
        assert_eq!(output.len(), 46);
    }

    #[test]
    fn audio_preparation_rejects_a_different_budget_domain() {
        let mut owner = AssetLoadBudget::default();
        let source =
            BudgetedMediaBytes::from_vec(minimal_pcm_wave(), "test audio source", &mut owner)
                .unwrap();
        let mut other = AssetLoadBudget::default();

        let Err(error) =
            PreparedAudioSource::prepare_source(AudioCompressionFormat::PCM, 0, source, &mut other)
        else {
            panic!("a different budget domain must not consume media bytes");
        };
        assert!(matches!(
            error,
            AudioSourceError::Budget(BudgetError::DomainMismatch {
                resource: "test audio source"
            })
        ));
    }

    #[test]
    fn fsb5_preparation_obeys_exact_caller_budget() {
        let mut measured = AssetLoadBudget::default();
        let prepared = prepare_short_vorbis(&mut measured).unwrap();
        let exact_bytes = measured.usage().bytes;
        assert!(exact_bytes > u64::try_from(SHORT_VORBIS.len()).unwrap());
        assert!(prepared.output.as_bytes().starts_with(b"OggS"));

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(prepare_short_vorbis(&mut exact).is_ok());
        assert_eq!(exact.usage().bytes, exact_bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            prepare_short_vorbis(&mut one_short),
            Err(AudioSourceError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn prepared_multi_page_fsb5_output_is_immutable_across_writes() {
        #[derive(Default)]
        struct CountingWriter {
            calls: usize,
            bytes: usize,
        }

        impl Write for CountingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                self.bytes = self
                    .bytes
                    .checked_add(bytes.len())
                    .ok_or_else(|| io::Error::other("test byte count overflow"))?;
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut budget = AssetLoadBudget::default();
        let prepared = prepare_short_vorbis(&mut budget).unwrap();
        let output = prepared.output.as_bytes();
        let output_pointer = output.as_ptr();
        let output_len = output.len();
        let page_count = output
            .windows(4)
            .filter(|window| *window == b"OggS")
            .count();
        assert!(
            page_count > 1,
            "fixture must exercise multi-page Ogg output"
        );

        let mut writer = CountingWriter::default();
        prepared.write_to(&mut writer).unwrap();
        prepared.write_to(&mut writer).unwrap();

        assert_eq!(writer.calls, 2);
        assert_eq!(writer.bytes, output_len * 2);
        assert_eq!(prepared.output.as_bytes().as_ptr(), output_pointer);
    }
}
