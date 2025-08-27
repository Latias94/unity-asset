//! Audio decoder module
//!
//! This module provides audio decoding capabilities using Symphonia
//! for various audio formats supported by Unity.

use super::formats::AudioCompressionFormat;
use super::types::{AudioClip, DecodedAudio};
use crate::error::{BinaryError, Result};

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
    #[cfg(feature = "symphonia")]
    pub fn decode(&self, clip: &AudioClip) -> Result<DecodedAudio> {
        use std::io::Cursor;
        use symphonia::core::audio::{AudioBufferRef, Signal};
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

            // Decode the packet into an audio buffer
            match decoder.decode(&packet) {
                Ok(decoded) => {
                    // Get audio buffer information
                    let spec = *decoded.spec();
                    sample_rate = spec.rate;
                    channels = spec.channels.count() as u32;

                    // Convert the audio buffer to f32 samples
                    match decoded {
                        AudioBufferRef::F32(buf) => {
                            samples.extend_from_slice(buf.chan(0));
                            if channels > 1 {
                                for ch in 1..channels as usize {
                                    if ch < buf.spec().channels.count() {
                                        let channel_samples = buf.chan(ch);
                                        // Interleave channels
                                        for (i, &sample) in channel_samples.iter().enumerate() {
                                            if i * channels as usize + ch < samples.len() {
                                                samples.insert(i * channels as usize + ch, sample);
                                            } else {
                                                samples.push(sample);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        AudioBufferRef::U8(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        let normalized = (sample as f32 - 128.0) / 128.0;
                                        samples.push(normalized);
                                    }
                                }
                            }
                        }
                        AudioBufferRef::U16(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        let normalized = (sample as f32 - 32768.0) / 32768.0;
                                        samples.push(normalized);
                                    }
                                }
                            }
                        }
                        AudioBufferRef::U32(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        let normalized =
                                            (sample as f32 - 2147483648.0) / 2147483648.0;
                                        samples.push(normalized);
                                    }
                                }
                            }
                        }
                        AudioBufferRef::S8(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        let normalized = sample as f32 / 128.0;
                                        samples.push(normalized);
                                    }
                                }
                            }
                        }
                        AudioBufferRef::S16(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        let normalized = sample as f32 / 32768.0;
                                        samples.push(normalized);
                                    }
                                }
                            }
                        }
                        AudioBufferRef::S32(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        let normalized = sample as f32 / 2147483648.0;
                                        samples.push(normalized);
                                    }
                                }
                            }
                        }
                        AudioBufferRef::F64(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        samples.push(sample as f32);
                                    }
                                }
                            }
                        }
                        AudioBufferRef::U24(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        let value = sample.inner() as i32;
                                        let normalized = (value as f32 - 8388608.0) / 8388608.0;
                                        samples.push(normalized);
                                    }
                                }
                            }
                        }
                        AudioBufferRef::S24(buf) => {
                            for ch in 0..channels as usize {
                                if ch < buf.spec().channels.count() {
                                    let channel_samples = buf.chan(ch);
                                    for &sample in channel_samples {
                                        let value = sample.inner();
                                        let normalized = value as f32 / 8388608.0;
                                        samples.push(normalized);
                                    }
                                }
                            }
                        }
                    }
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

        Ok(DecodedAudio::new(samples, sample_rate, channels))
    }

    /// Fallback decoder for when symphonia feature is not enabled
    #[cfg(not(feature = "symphonia"))]
    pub fn decode(&self, _clip: &AudioClip) -> Result<DecodedAudio> {
        Err(BinaryError::unsupported(
            "Audio decoding requires symphonia feature",
        ))
    }

    /// Check if a format can be decoded
    pub fn can_decode(&self, format: AudioCompressionFormat) -> bool {
        #[cfg(feature = "symphonia")]
        {
            matches!(
                format,
                AudioCompressionFormat::PCM
                    | AudioCompressionFormat::Vorbis
                    | AudioCompressionFormat::MP3
                    | AudioCompressionFormat::AAC
                    | AudioCompressionFormat::ADPCM
            )
        }

        #[cfg(not(feature = "symphonia"))]
        {
            false
        }
    }

    /// Get list of supported formats
    pub fn supported_formats(&self) -> Vec<AudioCompressionFormat> {
        #[cfg(feature = "symphonia")]
        {
            vec![
                AudioCompressionFormat::PCM,
                AudioCompressionFormat::Vorbis,
                AudioCompressionFormat::MP3,
                AudioCompressionFormat::AAC,
                AudioCompressionFormat::ADPCM,
            ]
        }

        #[cfg(not(feature = "symphonia"))]
        {
            vec![]
        }
    }
}

impl Default for AudioDecoder {
    fn default() -> Self {
        Self::new()
    }
}
