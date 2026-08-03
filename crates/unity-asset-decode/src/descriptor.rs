//! Closed media artifact descriptors shared by inspection and publication.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Current wire version of [`MediaDescriptor`].
pub const MEDIA_DESCRIPTOR_VERSION: u8 = 1;

/// Unity media family represented by an artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaFamily {
    Audio,
    Texture,
    Sprite,
}

/// Container observed at the source or produced by the prepared writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaContainer {
    Wave,
    Ogg,
    Fsb5,
    Mp3,
    AdtsAac,
    UnityTexture,
    Png,
}

/// Encoding observed at the source or produced by the prepared writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaEncoding {
    Pcm,
    Adpcm,
    Vorbis,
    Mp3,
    Aac,
    UnityTexture,
    Rgba8,
}

/// Supported Unity texture encodings with a strict PNG preparation path.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnityTextureEncoding {
    Alpha8,
    Argb4444,
    Rgb24,
    Rgba32,
    Argb32,
    Rgb565,
    Dxt1,
    Dxt5,
    Rgba4444,
    Bgra32,
    Bc4,
    Bc5,
    Bc7,
    Etc2Rgb,
    Etc2Rgba8,
    AstcRgba4x4,
    AstcRgba6x6,
    AstcRgba8x8,
}

/// Canonical destination suffix selected by a prepared media writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalMediaExtension {
    Wav,
    Ogg,
    Mp3,
    Aac,
    Png,
}

impl CanonicalMediaExtension {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Ogg => "ogg",
            Self::Mp3 => "mp3",
            Self::Aac => "aac",
            Self::Png => "png",
        }
    }
}

/// Canonical MIME selected by a prepared media writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaMime {
    AudioWav,
    AudioOgg,
    AudioMpeg,
    AudioAac,
    ImagePng,
}

impl MediaMime {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AudioWav => "audio/wav",
            Self::AudioOgg => "audio/ogg",
            Self::AudioMpeg => "audio/mpeg",
            Self::AudioAac => "audio/aac",
            Self::ImagePng => "image/png",
        }
    }
}

/// Checked dimensions carried by texture and sprite descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediaDimensions {
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaDimensionsWire {
    width: u32,
    height: u32,
}

impl<'de> Deserialize<'de> for MediaDimensions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MediaDimensionsWire::deserialize(deserializer)?;
        Self::new(wire.width, wire.height).map_err(serde::de::Error::custom)
    }
}

impl MediaDimensions {
    pub fn new(width: u32, height: u32) -> Result<Self, MediaDescriptorError> {
        if width == 0 || height == 0 {
            return Err(MediaDescriptorError::ZeroDimension);
        }
        width
            .checked_mul(height)
            .ok_or(MediaDescriptorError::DimensionOverflow { width, height })?;
        Ok(Self { width, height })
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// Bounded output size produced by a prepared writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediaOutputEstimate {
    upper_bound: u64,
    exact: bool,
}

impl MediaOutputEstimate {
    pub fn bounded(upper_bound: u64) -> Result<Self, MediaDescriptorError> {
        Self::new(upper_bound, false)
    }

    pub fn exact(bytes: u64) -> Result<Self, MediaDescriptorError> {
        Self::new(bytes, true)
    }

    fn new(upper_bound: u64, exact: bool) -> Result<Self, MediaDescriptorError> {
        if upper_bound == 0 {
            return Err(MediaDescriptorError::ZeroOutputBound);
        }
        Ok(Self { upper_bound, exact })
    }

    #[must_use]
    pub const fn upper_bound(self) -> u64 {
        self.upper_bound
    }

    #[must_use]
    pub const fn is_exact(self) -> bool {
        self.exact
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaOutputEstimateWire {
    upper_bound: u64,
    exact: bool,
}

impl<'de> Deserialize<'de> for MediaOutputEstimate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MediaOutputEstimateWire::deserialize(deserializer)?;
        Self::new(wire.upper_bound, wire.exact).map_err(serde::de::Error::custom)
    }
}

/// Validated source form for a playable audio artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparedAudioSourceKind {
    WavePcm,
    WaveAdpcm,
    Fsb5Vorbis,
    Mp3,
    AdtsAac,
}

impl PreparedAudioSourceKind {
    const fn source_container(self) -> MediaContainer {
        match self {
            Self::WavePcm | Self::WaveAdpcm => MediaContainer::Wave,
            Self::Fsb5Vorbis => MediaContainer::Fsb5,
            Self::Mp3 => MediaContainer::Mp3,
            Self::AdtsAac => MediaContainer::AdtsAac,
        }
    }

    const fn source_encoding(self) -> MediaEncoding {
        match self {
            Self::WavePcm => MediaEncoding::Pcm,
            Self::WaveAdpcm => MediaEncoding::Adpcm,
            Self::Fsb5Vorbis => MediaEncoding::Vorbis,
            Self::Mp3 => MediaEncoding::Mp3,
            Self::AdtsAac => MediaEncoding::Aac,
        }
    }

    const fn output_container(self) -> MediaContainer {
        match self {
            Self::WavePcm | Self::WaveAdpcm => MediaContainer::Wave,
            Self::Fsb5Vorbis => MediaContainer::Ogg,
            Self::Mp3 => MediaContainer::Mp3,
            Self::AdtsAac => MediaContainer::AdtsAac,
        }
    }

    const fn canonical_extension(self) -> CanonicalMediaExtension {
        match self {
            Self::WavePcm | Self::WaveAdpcm => CanonicalMediaExtension::Wav,
            Self::Fsb5Vorbis => CanonicalMediaExtension::Ogg,
            Self::Mp3 => CanonicalMediaExtension::Mp3,
            Self::AdtsAac => CanonicalMediaExtension::Aac,
        }
    }

    const fn mime(self) -> MediaMime {
        match self {
            Self::WavePcm | Self::WaveAdpcm => MediaMime::AudioWav,
            Self::Fsb5Vorbis => MediaMime::AudioOgg,
            Self::Mp3 => MediaMime::AudioMpeg,
            Self::AdtsAac => MediaMime::AudioAac,
        }
    }
}

/// Closed descriptor emitted only after source bytes have been strictly prepared.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediaDescriptor {
    version: u8,
    family: MediaFamily,
    source_container: MediaContainer,
    source_encoding: MediaEncoding,
    texture_encoding: Option<UnityTextureEncoding>,
    output_container: MediaContainer,
    output_encoding: MediaEncoding,
    canonical_extension: CanonicalMediaExtension,
    mime: MediaMime,
    input_bytes: u64,
    output: MediaOutputEstimate,
    dimensions: Option<MediaDimensions>,
}

impl MediaDescriptor {
    pub fn audio(
        source: PreparedAudioSourceKind,
        input_bytes: u64,
        output: MediaOutputEstimate,
    ) -> Result<Self, MediaDescriptorError> {
        validate_input_bytes(input_bytes)?;
        Ok(Self {
            version: MEDIA_DESCRIPTOR_VERSION,
            family: MediaFamily::Audio,
            source_container: source.source_container(),
            source_encoding: source.source_encoding(),
            texture_encoding: None,
            output_container: source.output_container(),
            output_encoding: source.source_encoding(),
            canonical_extension: source.canonical_extension(),
            mime: source.mime(),
            input_bytes,
            output,
            dimensions: None,
        })
    }

    pub fn texture_png(
        encoding: UnityTextureEncoding,
        dimensions: MediaDimensions,
        input_bytes: u64,
        output: MediaOutputEstimate,
    ) -> Result<Self, MediaDescriptorError> {
        Self::png(
            MediaFamily::Texture,
            encoding,
            dimensions,
            input_bytes,
            output,
        )
    }

    pub fn sprite_png(
        encoding: UnityTextureEncoding,
        dimensions: MediaDimensions,
        input_bytes: u64,
        output: MediaOutputEstimate,
    ) -> Result<Self, MediaDescriptorError> {
        Self::png(
            MediaFamily::Sprite,
            encoding,
            dimensions,
            input_bytes,
            output,
        )
    }

    fn png(
        family: MediaFamily,
        encoding: UnityTextureEncoding,
        dimensions: MediaDimensions,
        input_bytes: u64,
        output: MediaOutputEstimate,
    ) -> Result<Self, MediaDescriptorError> {
        validate_input_bytes(input_bytes)?;
        Ok(Self {
            version: MEDIA_DESCRIPTOR_VERSION,
            family,
            source_container: MediaContainer::UnityTexture,
            source_encoding: MediaEncoding::UnityTexture,
            texture_encoding: Some(encoding),
            output_container: MediaContainer::Png,
            output_encoding: MediaEncoding::Rgba8,
            canonical_extension: CanonicalMediaExtension::Png,
            mime: MediaMime::ImagePng,
            input_bytes,
            output,
            dimensions: Some(dimensions),
        })
    }

    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    #[must_use]
    pub const fn family(&self) -> MediaFamily {
        self.family
    }

    #[must_use]
    pub const fn source_container(&self) -> MediaContainer {
        self.source_container
    }

    #[must_use]
    pub const fn source_encoding(&self) -> MediaEncoding {
        self.source_encoding
    }

    #[must_use]
    pub const fn texture_encoding(&self) -> Option<UnityTextureEncoding> {
        self.texture_encoding
    }

    #[must_use]
    pub const fn output_container(&self) -> MediaContainer {
        self.output_container
    }

    #[must_use]
    pub const fn output_encoding(&self) -> MediaEncoding {
        self.output_encoding
    }

    #[must_use]
    pub const fn canonical_extension(&self) -> CanonicalMediaExtension {
        self.canonical_extension
    }

    #[must_use]
    pub const fn mime(&self) -> MediaMime {
        self.mime
    }

    #[must_use]
    pub const fn input_bytes(&self) -> u64 {
        self.input_bytes
    }

    #[must_use]
    pub const fn output(&self) -> MediaOutputEstimate {
        self.output
    }

    #[must_use]
    pub const fn dimensions(&self) -> Option<MediaDimensions> {
        self.dimensions
    }

    fn validate(self) -> Result<Self, MediaDescriptorError> {
        if self.version != MEDIA_DESCRIPTOR_VERSION {
            return Err(MediaDescriptorError::UnsupportedVersion(self.version));
        }
        validate_input_bytes(self.input_bytes)?;
        match self.family {
            MediaFamily::Audio => self.validate_audio(),
            MediaFamily::Texture | MediaFamily::Sprite => self.validate_png(),
        }?;
        Ok(self)
    }

    fn validate_audio(&self) -> Result<(), MediaDescriptorError> {
        if self.texture_encoding.is_some() || self.dimensions.is_some() {
            return Err(MediaDescriptorError::InvalidShape(
                "audio descriptors cannot carry texture metadata",
            ));
        }
        let expected = match (self.source_container, self.source_encoding) {
            (MediaContainer::Wave, MediaEncoding::Pcm) => PreparedAudioSourceKind::WavePcm,
            (MediaContainer::Wave, MediaEncoding::Adpcm) => PreparedAudioSourceKind::WaveAdpcm,
            (MediaContainer::Fsb5, MediaEncoding::Vorbis) => PreparedAudioSourceKind::Fsb5Vorbis,
            (MediaContainer::Mp3, MediaEncoding::Mp3) => PreparedAudioSourceKind::Mp3,
            (MediaContainer::AdtsAac, MediaEncoding::Aac) => PreparedAudioSourceKind::AdtsAac,
            _ => {
                return Err(MediaDescriptorError::InvalidShape(
                    "audio source container and encoding disagree",
                ));
            }
        };
        if self.output_container != expected.output_container()
            || self.output_encoding != expected.source_encoding()
            || self.canonical_extension != expected.canonical_extension()
            || self.mime != expected.mime()
        {
            return Err(MediaDescriptorError::InvalidShape(
                "audio output identity disagrees with its prepared source",
            ));
        }
        Ok(())
    }

    fn validate_png(&self) -> Result<(), MediaDescriptorError> {
        if self.source_container != MediaContainer::UnityTexture
            || self.source_encoding != MediaEncoding::UnityTexture
            || self.texture_encoding.is_none()
            || self.output_container != MediaContainer::Png
            || self.output_encoding != MediaEncoding::Rgba8
            || self.canonical_extension != CanonicalMediaExtension::Png
            || self.mime != MediaMime::ImagePng
            || self.dimensions.is_none()
        {
            return Err(MediaDescriptorError::InvalidShape(
                "texture and sprite descriptors must describe Unity texture data to PNG",
            ));
        }
        Ok(())
    }
}

fn validate_input_bytes(input_bytes: u64) -> Result<(), MediaDescriptorError> {
    if input_bytes == 0 {
        Err(MediaDescriptorError::ZeroInputBytes)
    } else {
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaDescriptorWire {
    version: u8,
    family: MediaFamily,
    source_container: MediaContainer,
    source_encoding: MediaEncoding,
    texture_encoding: Option<UnityTextureEncoding>,
    output_container: MediaContainer,
    output_encoding: MediaEncoding,
    canonical_extension: CanonicalMediaExtension,
    mime: MediaMime,
    input_bytes: u64,
    output: MediaOutputEstimate,
    dimensions: Option<MediaDimensions>,
}

impl<'de> Deserialize<'de> for MediaDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MediaDescriptorWire::deserialize(deserializer)?;
        Self {
            version: wire.version,
            family: wire.family,
            source_container: wire.source_container,
            source_encoding: wire.source_encoding,
            texture_encoding: wire.texture_encoding,
            output_container: wire.output_container,
            output_encoding: wire.output_encoding,
            canonical_extension: wire.canonical_extension,
            mime: wire.mime,
            input_bytes: wire.input_bytes,
            output: wire.output,
            dimensions: wire.dimensions,
        }
        .validate()
        .map_err(serde::de::Error::custom)
    }
}

/// Invalid or internally inconsistent media descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MediaDescriptorError {
    #[error("unsupported media descriptor version {0}")]
    UnsupportedVersion(u8),
    #[error("media descriptor input must be non-empty")]
    ZeroInputBytes,
    #[error("media descriptor output bound must be non-zero")]
    ZeroOutputBound,
    #[error("media dimensions must be non-zero")]
    ZeroDimension,
    #[error("media dimensions {width}x{height} overflow their pixel domain")]
    DimensionOverflow { width: u32, height: u32 },
    #[error("invalid media descriptor shape: {0}")]
    InvalidShape(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_descriptor_is_self_consistent() {
        let descriptor = MediaDescriptor::audio(
            PreparedAudioSourceKind::Fsb5Vorbis,
            128,
            MediaOutputEstimate::exact(256).unwrap(),
        )
        .unwrap();

        assert_eq!(descriptor.family(), MediaFamily::Audio);
        assert_eq!(descriptor.source_container(), MediaContainer::Fsb5);
        assert_eq!(descriptor.output_container(), MediaContainer::Ogg);
        assert_eq!(descriptor.canonical_extension().as_str(), "ogg");
        assert_eq!(descriptor.mime().as_str(), "audio/ogg");
        assert!(descriptor.output().is_exact());
    }

    #[test]
    fn descriptor_deserialization_rejects_every_tampered_output_identity_field() {
        let descriptor = MediaDescriptor::audio(
            PreparedAudioSourceKind::Mp3,
            4,
            MediaOutputEstimate::exact(4).unwrap(),
        )
        .unwrap();
        let encoded = serde_json::to_value(descriptor).unwrap();
        for (field, value) in [
            ("output_container", "wave"),
            ("output_encoding", "pcm"),
            ("canonical_extension", "wav"),
            ("mime", "audio_wav"),
        ] {
            let mut tampered = encoded.clone();
            tampered[field] = serde_json::Value::String(value.to_owned());
            let error = serde_json::from_value::<MediaDescriptor>(tampered).unwrap_err();
            assert!(
                error.to_string().contains("output identity"),
                "{field} produced an unrelated error: {error}"
            );
        }
    }

    #[test]
    fn descriptor_rejects_direct_ogg_without_a_strict_preparer() {
        let descriptor = MediaDescriptor::audio(
            PreparedAudioSourceKind::Fsb5Vorbis,
            128,
            MediaOutputEstimate::exact(256).unwrap(),
        )
        .unwrap();
        let mut encoded = serde_json::to_value(descriptor).unwrap();
        encoded["source_container"] = serde_json::Value::String("ogg".to_owned());

        let error = serde_json::from_value::<MediaDescriptor>(encoded).unwrap_err();

        assert!(error.to_string().contains("source container and encoding"));
    }

    #[test]
    fn png_descriptor_deserialization_rejects_tampered_media_identity() {
        for descriptor in [
            MediaDescriptor::texture_png(
                UnityTextureEncoding::Rgba32,
                MediaDimensions::new(2, 2).unwrap(),
                16,
                MediaOutputEstimate::bounded(1024).unwrap(),
            )
            .unwrap(),
            MediaDescriptor::sprite_png(
                UnityTextureEncoding::Rgba32,
                MediaDimensions::new(2, 2).unwrap(),
                16,
                MediaOutputEstimate::bounded(1024).unwrap(),
            )
            .unwrap(),
        ] {
            let encoded = serde_json::to_value(descriptor).unwrap();
            for (field, value) in [
                ("source_container", "png"),
                ("source_encoding", "rgba8"),
                ("output_container", "unity_texture"),
                ("output_encoding", "unity_texture"),
                ("canonical_extension", "wav"),
                ("mime", "audio_wav"),
            ] {
                let mut tampered = encoded.clone();
                tampered[field] = serde_json::Value::String(value.to_owned());
                let error = serde_json::from_value::<MediaDescriptor>(tampered).unwrap_err();
                assert!(
                    error.to_string().contains("Unity texture data to PNG"),
                    "{field} produced an unrelated error: {error}"
                );
            }
        }
    }

    #[test]
    fn descriptor_deserialization_rejects_zero_bounds_and_dimensions() {
        let descriptor = MediaDescriptor::texture_png(
            UnityTextureEncoding::Rgba32,
            MediaDimensions::new(2, 2).unwrap(),
            16,
            MediaOutputEstimate::bounded(1024).unwrap(),
        )
        .unwrap();
        let encoded = serde_json::to_value(descriptor).unwrap();

        let mut zero_output = encoded.clone();
        zero_output["output"]["upper_bound"] = serde_json::Value::from(0);
        assert!(serde_json::from_value::<MediaDescriptor>(zero_output).is_err());

        let mut zero_width = encoded;
        zero_width["dimensions"]["width"] = serde_json::Value::from(0);
        assert!(serde_json::from_value::<MediaDescriptor>(zero_width).is_err());
    }
}
