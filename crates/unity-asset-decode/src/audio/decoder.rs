//! Audio decoder module
//!
//! This module provides audio decoding capabilities using Symphonia
//! for various audio formats supported by Unity.

use super::formats::AudioCompressionFormat;
use super::ogg::final_ogg_granule;
use super::types::{AudioClip, DecodedAudio};
use unity_asset_binary::{BinaryError, Result};

/// Main audio decoder
///
/// This struct provides methods for decoding various audio formats
/// using the Symphonia audio library.
pub struct AudioDecoder;

impl AudioDecoder {
    /// Create a new audio decoder
    pub fn new() -> Self {
        Self
    }

    /// Decode audio using Symphonia (supports many formats)
    pub fn decode(&self, clip: &AudioClip) -> Result<DecodedAudio> {
        use std::io::Cursor;
        use symphonia::core::audio::SampleBuffer;
        use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
        use symphonia::core::errors::Error as SymphoniaError;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::core::probe::Hint;

        if clip.data.is_empty() {
            return Err(BinaryError::invalid_data("No audio data to decode"));
        }

        // Create a media source from the audio data
        let cursor = Cursor::new(clip.data.clone());
        let media_source = MediaSourceStream::new(Box::new(cursor), Default::default());

        // Create a probe hint based on the compression format
        let mut hint = Hint::new();
        match clip.compression_format() {
            AudioCompressionFormat::Vorbis => hint.with_extension("ogg"),
            AudioCompressionFormat::MP3 => hint.with_extension("mp3"),
            AudioCompressionFormat::AAC => hint.with_extension("aac"),
            AudioCompressionFormat::PCM => hint.with_extension("wav"),
            _ => &mut hint,
        };

        // Get the metadata and format readers
        let meta_opts: MetadataOptions = Default::default();
        let fmt_opts: FormatOptions = Default::default();

        // Probe the media source
        let probed = symphonia::default::get_probe()
            .format(&hint, media_source, &fmt_opts, &meta_opts)
            .map_err(|e| BinaryError::generic(format!("Failed to probe audio format: {}", e)))?;

        // Get the instantiated format reader
        let mut format = probed.format;

        // Find the first audio track with a known (decodeable) codec
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| BinaryError::generic("No supported audio tracks found"))?;

        // Use the default options for the decoder
        let dec_opts: DecoderOptions = Default::default();

        // Create a decoder for the track
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &dec_opts)
            .map_err(|e| BinaryError::generic(format!("Failed to create decoder: {}", e)))?;

        // Store the track identifier, it will be used to filter packets
        let track_id = track.id;

        let mut samples = Vec::new();
        let mut sample_rate = 44100u32;
        let mut channels = 2u32;

        // The decode loop
        loop {
            // Get the next packet from the media format
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::ResetRequired) => {
                    // The track list has been changed. Re-examine it and create a new set of decoders,
                    // then restart the decode loop. This is an advanced feature and it is not
                    // unreasonable to consider this "the end of the stream". As of v0.5.0, the only
                    // usage of this is for chained OGG physical streams.
                    break;
                }
                Err(SymphoniaError::IoError(_)) => {
                    // The packet reader has reached the end of the stream
                    break;
                }
                Err(err) => {
                    // A unrecoverable error occurred, halt decoding
                    return Err(BinaryError::generic(format!("Decode error: {}", err)));
                }
            };

            // Consume any new metadata that has been read since the last packet
            while !format.metadata().is_latest() {
                // Pop the latest metadata and consume it
                format.metadata().pop();
            }

            // If the packet does not belong to the selected track, skip over it
            if packet.track_id() != track_id {
                continue;
            }

            // Decode each packet into an interleaved f32 buffer. Symphonia's
            // conversion preserves channel order for every supported sample type.
            match decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    sample_rate = spec.rate;
                    channels = spec.channels.count() as u32;
                    let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                    buffer.copy_interleaved_ref(decoded);
                    samples.extend_from_slice(buffer.samples());
                }
                Err(SymphoniaError::IoError(_)) => {
                    // The packet reader has reached the end of the stream
                    break;
                }
                Err(SymphoniaError::DecodeError(_)) => {
                    // Decode error, try to continue
                    continue;
                }
                Err(err) => {
                    // A unrecoverable error occurred, halt decoding
                    return Err(BinaryError::generic(format!("Decode error: {}", err)));
                }
            }
        }

        if samples.is_empty() {
            return Err(BinaryError::generic("No audio samples decoded"));
        }
        trim_to_ogg_granule(clip, &mut samples, channels);

        Ok(DecodedAudio::new(samples, sample_rate, channels))
    }

    /// Check if a format can be decoded
    pub fn can_decode(&self, format: AudioCompressionFormat) -> bool {
        matches!(
            format,
            AudioCompressionFormat::PCM
                | AudioCompressionFormat::Vorbis
                | AudioCompressionFormat::MP3
                | AudioCompressionFormat::AAC
                | AudioCompressionFormat::ADPCM
        )
    }

    /// Get list of supported formats
    pub fn supported_formats(&self) -> Vec<AudioCompressionFormat> {
        vec![
            AudioCompressionFormat::PCM,
            AudioCompressionFormat::Vorbis,
            AudioCompressionFormat::MP3,
            AudioCompressionFormat::AAC,
            AudioCompressionFormat::ADPCM,
        ]
    }
}

fn trim_to_ogg_granule(clip: &AudioClip, samples: &mut Vec<f32>, channels: u32) {
    if clip.compression_format() != AudioCompressionFormat::Vorbis || channels == 0 {
        return;
    }
    let Some(granule) = final_ogg_granule(&clip.data) else {
        return;
    };
    let Ok(frames) = usize::try_from(granule) else {
        return;
    };
    let Some(sample_count) = frames.checked_mul(channels as usize) else {
        return;
    };
    if sample_count <= samples.len() {
        samples.truncate(sample_count);
    }
}

impl Default for AudioDecoder {
    fn default() -> Self {
        Self::new()
    }
}
