//! Validated representation lifecycle persisted by an extraction plan.

#[cfg(feature = "decode")]
use std::io;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use unity_asset_binary::asset::class_ids;
#[cfg(feature = "decode")]
use unity_asset_core::AssetLoadBudget;
use unity_asset_core::{ObjectAddress, ObjectKind, SourceKind, SourceLocator};
use unity_asset_decode::descriptor::{MediaDescriptor, MediaFamily};

use super::super::contract::{
    ExtractionArtifactKind, ExtractionDiagnostic, ExtractionDiagnosticCode, ExtractionPath,
    ExtractionRepresentationPolicy, ExtractionSourceExpectation,
};
use crate::workspace::StreamedResourceRequest;
#[cfg(feature = "decode")]
use crate::workspace::{
    ResolvedStreamedResource, WorkspaceByteRange, WorkspaceError, WorkspaceLookup, WorkspaceView,
};

/// Exact sidecar and byte range selected while planning streamed media.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(in crate::extraction) struct PlannedStreamSource {
    request: StreamedResourceRequest,
    source: ExtractionSourceExpectation,
}

impl PlannedStreamSource {
    pub(in crate::extraction) fn new(
        request: StreamedResourceRequest,
        source: ExtractionSourceExpectation,
    ) -> Result<Self, RepresentationContractError> {
        let actual = source.fingerprint().kind();
        if actual != SourceKind::StreamedResource {
            return Err(RepresentationContractError::SourceKindMismatch {
                locator: source.locator().clone(),
                expected: SourceKind::StreamedResource,
                actual,
            });
        }
        Ok(Self { request, source })
    }

    pub(in crate::extraction) const fn request(&self) -> &StreamedResourceRequest {
        &self.request
    }

    pub(in crate::extraction) const fn source(&self) -> &ExtractionSourceExpectation {
        &self.source
    }

    #[cfg(feature = "decode")]
    pub(in crate::extraction) fn matches_resolution(
        &self,
        resource: &ResolvedStreamedResource,
    ) -> bool {
        resource.source().locator() == self.source.locator()
            && resource.source().fingerprint() == self.source.fingerprint()
            && resource.offset() == self.request.offset()
            && resource.size() == self.request.size()
    }

    #[cfg(feature = "decode")]
    pub(in crate::extraction) fn open(
        &self,
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceByteRange, WorkspaceError> {
        let source = match view.resolve_source(self.source.locator(), budget)? {
            WorkspaceLookup::Resolved(source) => source,
            WorkspaceLookup::Unloaded
            | WorkspaceLookup::Missing
            | WorkspaceLookup::Ambiguous { .. }
            | WorkspaceLookup::Invalid { .. } => {
                return Err(WorkspaceError::operation(
                    "planned streamed-resource source validation",
                    io::Error::other("planned streamed-resource source is unavailable"),
                ));
            }
        };
        if source.kind() != SourceKind::StreamedResource
            || source.locator() != self.source.locator()
            || source.fingerprint() != self.source.fingerprint()
        {
            return Err(WorkspaceError::ObservedSourceChanged {
                source_id: Box::new(source.id()),
                expected: self.source.fingerprint(),
                actual: source.fingerprint(),
            });
        }
        view.read_source_range(
            source.id(),
            self.request.offset(),
            self.request.size(),
            budget,
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedStreamSourceWire {
    request: StreamedResourceRequest,
    source: ExtractionSourceExpectation,
}

impl<'de> Deserialize<'de> for PlannedStreamSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PlannedStreamSourceWire::deserialize(deserializer)?;
        Self::new(wire.request, wire.source).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::extraction) struct PlannedFallback {
    path: ExtractionPath,
    content: PlannedContent,
    representation_semantics: RepresentationSemantics,
}

impl PlannedFallback {
    pub(in crate::extraction) fn new(
        path: ExtractionPath,
        content: PlannedContent,
    ) -> Result<Self, RepresentationContractError> {
        if !matches!(content, PlannedContent::RawBinary) {
            return Err(RepresentationContractError::InvalidFallbackContent);
        }
        content.validate_destination(&path)?;
        Ok(Self {
            path,
            representation_semantics: content.current_semantics(),
            content,
        })
    }

    pub(in crate::extraction) fn from_declared_parts(
        kind: ExtractionArtifactKind,
        path: ExtractionPath,
        content: PlannedContent,
        representation_semantics: Option<RepresentationSemantics>,
    ) -> Result<Self, RepresentationContractError> {
        content.validate_declared_kind(kind)?;
        let representation_semantics = representation_semantics.ok_or(
            RepresentationContractError::MissingRepresentationSemantics {
                artifact_kind: content.artifact_kind(),
            },
        )?;
        content.validate_semantics(representation_semantics)?;
        let mut fallback = Self::new(path, content)?;
        fallback.representation_semantics = representation_semantics;
        Ok(fallback)
    }

    pub(in crate::extraction) const fn kind(&self) -> ExtractionArtifactKind {
        self.content.artifact_kind()
    }

    pub(in crate::extraction) const fn path(&self) -> &ExtractionPath {
        &self.path
    }

    pub(in crate::extraction) const fn content(&self) -> &PlannedContent {
        &self.content
    }

    pub(in crate::extraction) const fn representation_semantics(&self) -> RepresentationSemantics {
        self.representation_semantics
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::extraction) enum RawBinaryBytesSemantics {
    WorkspaceObjectRawBytesV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::extraction) enum YamlSerializerSemantics {
    UnityYamlSerializerCanonicalV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::extraction) enum TextAssetBytesSemantics {
    TypeTreeScriptBytesV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::extraction) enum AudioPreparationSemantics {
    PreparedStandardAudioSourceV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::extraction) enum PngPixelSemantics {
    TopLeftRgba8V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::extraction) enum PlatformTransformSemantics {
    SerializedFileBuildTargetClosedV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::extraction) enum PngEncoderSemantics {
    FilterNoneStoredDeflateBlockPerIdatRgba8V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::extraction) enum SpriteCropSemantics {
    TopLeftTextureSpaceV1,
}

/// Closed identity of the implementation semantics required to reproduce one representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(in crate::extraction) enum RepresentationSemantics {
    RawBinary {
        bytes: RawBinaryBytesSemantics,
    },
    Yaml {
        serializer: YamlSerializerSemantics,
    },
    TextAsset {
        bytes: TextAssetBytesSemantics,
    },
    Audio {
        preparation: AudioPreparationSemantics,
    },
    TexturePng {
        pixels: PngPixelSemantics,
        platform_transform: PlatformTransformSemantics,
        encoder: PngEncoderSemantics,
    },
    SpritePng {
        pixels: PngPixelSemantics,
        platform_transform: PlatformTransformSemantics,
        crop: SpriteCropSemantics,
        encoder: PngEncoderSemantics,
    },
}

impl RepresentationSemantics {
    const fn raw_binary() -> Self {
        Self::RawBinary {
            bytes: RawBinaryBytesSemantics::WorkspaceObjectRawBytesV1,
        }
    }

    const fn yaml() -> Self {
        Self::Yaml {
            serializer: YamlSerializerSemantics::UnityYamlSerializerCanonicalV1,
        }
    }

    const fn text_asset() -> Self {
        Self::TextAsset {
            bytes: TextAssetBytesSemantics::TypeTreeScriptBytesV1,
        }
    }

    const fn audio() -> Self {
        Self::Audio {
            preparation: AudioPreparationSemantics::PreparedStandardAudioSourceV1,
        }
    }

    const fn texture_png() -> Self {
        Self::TexturePng {
            pixels: PngPixelSemantics::TopLeftRgba8V1,
            platform_transform: PlatformTransformSemantics::SerializedFileBuildTargetClosedV1,
            encoder: PngEncoderSemantics::FilterNoneStoredDeflateBlockPerIdatRgba8V1,
        }
    }

    const fn sprite_png() -> Self {
        Self::SpritePng {
            pixels: PngPixelSemantics::TopLeftRgba8V1,
            platform_transform: PlatformTransformSemantics::SerializedFileBuildTargetClosedV1,
            crop: SpriteCropSemantics::TopLeftTextureSpaceV1,
            encoder: PngEncoderSemantics::FilterNoneStoredDeflateBlockPerIdatRgba8V1,
        }
    }
}

/// Inert representation selected during extraction planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(in crate::extraction) enum PlannedContent {
    RawBinary,
    Yaml,
    TextAsset,
    Audio {
        stream: Option<PlannedStreamSource>,
        descriptor: MediaDescriptor,
    },
    TexturePng {
        stream: Option<PlannedStreamSource>,
        descriptor: MediaDescriptor,
    },
    SpritePng {
        texture: ObjectAddress,
        texture_stream: Option<PlannedStreamSource>,
        descriptor: MediaDescriptor,
    },
}

impl PlannedContent {
    pub(in crate::extraction) const fn current_semantics(&self) -> RepresentationSemantics {
        match self {
            Self::RawBinary => RepresentationSemantics::raw_binary(),
            Self::Yaml => RepresentationSemantics::yaml(),
            Self::TextAsset => RepresentationSemantics::text_asset(),
            Self::Audio { .. } => RepresentationSemantics::audio(),
            Self::TexturePng { .. } => RepresentationSemantics::texture_png(),
            Self::SpritePng { .. } => RepresentationSemantics::sprite_png(),
        }
    }

    fn validate_semantics(
        &self,
        declared: RepresentationSemantics,
    ) -> Result<(), RepresentationContractError> {
        let expected = self.current_semantics();
        if declared != expected {
            return Err(
                RepresentationContractError::RepresentationSemanticsMismatch { declared, expected },
            );
        }
        Ok(())
    }

    pub(in crate::extraction) const fn artifact_kind(&self) -> ExtractionArtifactKind {
        match self {
            Self::RawBinary => ExtractionArtifactKind::BinaryRaw,
            Self::Yaml => ExtractionArtifactKind::Yaml,
            Self::TextAsset => ExtractionArtifactKind::Text,
            Self::Audio { .. } => ExtractionArtifactKind::Audio,
            Self::TexturePng { .. } => ExtractionArtifactKind::TexturePng,
            Self::SpritePng { .. } => ExtractionArtifactKind::SpritePng,
        }
    }

    fn validate(&self) -> Result<(), RepresentationContractError> {
        let family = match self {
            Self::Audio { descriptor, .. } => Some((MediaFamily::Audio, descriptor.family())),
            Self::TexturePng { descriptor, .. } => {
                Some((MediaFamily::Texture, descriptor.family()))
            }
            Self::SpritePng { descriptor, .. } => Some((MediaFamily::Sprite, descriptor.family())),
            Self::RawBinary | Self::Yaml | Self::TextAsset => None,
        };
        if let Some((expected, actual)) = family
            && actual != expected
        {
            return Err(RepresentationContractError::MediaDescriptorFamilyMismatch {
                expected,
                actual,
            });
        }
        if let Some(stream) = self.stream_source() {
            let actual = stream.source().fingerprint().kind();
            if actual != SourceKind::StreamedResource {
                return Err(RepresentationContractError::SourceKindMismatch {
                    locator: stream.source().locator().clone(),
                    expected: SourceKind::StreamedResource,
                    actual,
                });
            }
        }
        Ok(())
    }

    pub(in crate::extraction) fn validate_declared_kind(
        &self,
        declared: ExtractionArtifactKind,
    ) -> Result<(), RepresentationContractError> {
        self.validate()?;
        let actual = self.artifact_kind();
        if declared != actual {
            return Err(RepresentationContractError::ArtifactKindMismatch { declared, actual });
        }
        Ok(())
    }

    fn validate_destination(
        &self,
        path: &ExtractionPath,
    ) -> Result<(), RepresentationContractError> {
        let expected = self.canonical_extension();
        let actual = path
            .as_str()
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| extension);
        if actual != Some(expected) {
            return Err(RepresentationContractError::ArtifactExtensionMismatch {
                path: path.as_str().to_owned(),
                expected,
            });
        }
        Ok(())
    }

    pub(in crate::extraction) fn canonical_extension(&self) -> &'static str {
        match self {
            Self::RawBinary => "bin",
            Self::Yaml => "yaml",
            Self::TextAsset => "txt",
            Self::Audio { descriptor, .. }
            | Self::TexturePng { descriptor, .. }
            | Self::SpritePng { descriptor, .. } => descriptor.canonical_extension().as_str(),
        }
    }

    pub(in crate::extraction) const fn stream_source(&self) -> Option<&PlannedStreamSource> {
        match self {
            Self::Audio { stream, .. } | Self::TexturePng { stream, .. } => stream.as_ref(),
            Self::SpritePng { texture_stream, .. } => texture_stream.as_ref(),
            Self::RawBinary | Self::Yaml | Self::TextAsset => None,
        }
    }

    const fn requires_write_budget(&self) -> bool {
        matches!(self, Self::Yaml)
    }

    fn validate_request(
        &self,
        object_kind: ObjectKind,
        class_id: i32,
        policy: ExtractionRepresentationPolicy,
    ) -> Result<(), RepresentationContractError> {
        let content_matches_object = match object_kind {
            ObjectKind::Binary => !matches!(self, Self::Yaml),
            ObjectKind::Yaml => matches!(self, Self::Yaml),
        };
        if !content_matches_object {
            return Err(RepresentationContractError::ObjectKindContentMismatch {
                object_kind,
                artifact_kind: self.artifact_kind(),
            });
        }
        let content_matches_class = match self {
            Self::RawBinary | Self::Yaml => true,
            Self::TextAsset => class_id == class_ids::TEXT_ASSET,
            Self::Audio { .. } => class_id == class_ids::AUDIO_CLIP,
            Self::TexturePng { .. } => class_id == class_ids::TEXTURE_2D,
            Self::SpritePng { .. } => class_id == class_ids::SPRITE,
        };
        if !content_matches_class {
            return Err(RepresentationContractError::ClassContentMismatch {
                class_id,
                artifact_kind: self.artifact_kind(),
            });
        }
        let policy_matches = match policy {
            ExtractionRepresentationPolicy::RawOnly => {
                matches!(self, Self::RawBinary | Self::Yaml)
            }
            ExtractionRepresentationPolicy::PreferDecoded => true,
            ExtractionRepresentationPolicy::RequireDecoded => !matches!(self, Self::RawBinary),
        };
        if !policy_matches {
            return Err(RepresentationContractError::RepresentationPolicyMismatch {
                policy,
                artifact_kind: self.artifact_kind(),
            });
        }
        Ok(())
    }

    const fn dependency_locator(&self) -> Option<&SourceLocator> {
        match self {
            Self::SpritePng { texture, .. } => Some(texture.source_locator()),
            Self::RawBinary
            | Self::Yaml
            | Self::TextAsset
            | Self::Audio { .. }
            | Self::TexturePng { .. } => None,
        }
    }
}

pub(in crate::extraction) struct RepresentationContractParts {
    pub preferred_path: ExtractionPath,
    pub preferred_content: PlannedContent,
    pub fallback: Option<PlannedFallback>,
    pub working_set_bytes: u64,
    pub diagnostics: Vec<ExtractionDiagnostic>,
}

/// Complete, validated representation lifecycle for one planned artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::extraction) struct RepresentationContract {
    preferred_path: ExtractionPath,
    preferred_content: PlannedContent,
    representation_semantics: RepresentationSemantics,
    fallback: Option<PlannedFallback>,
    working_set_bytes: u64,
    diagnostics: Box<[ExtractionDiagnostic]>,
}

impl RepresentationContract {
    pub(in crate::extraction) fn from_parts(
        ordinal: u32,
        address: &ObjectAddress,
        parts: RepresentationContractParts,
    ) -> Result<Self, RepresentationContractError> {
        let representation_semantics = parts.preferred_content.current_semantics();
        Self::from_declared_parts(ordinal, address, representation_semantics, parts)
    }

    pub(in crate::extraction) fn from_declared_parts(
        ordinal: u32,
        address: &ObjectAddress,
        representation_semantics: RepresentationSemantics,
        parts: RepresentationContractParts,
    ) -> Result<Self, RepresentationContractError> {
        let RepresentationContractParts {
            preferred_path,
            preferred_content,
            fallback,
            working_set_bytes,
            mut diagnostics,
        } = parts;
        preferred_content.validate()?;
        preferred_content.validate_semantics(representation_semantics)?;
        preferred_content.validate_destination(&preferred_path)?;
        if let Some(fallback) = fallback.as_ref() {
            if matches!(
                preferred_content.artifact_kind(),
                ExtractionArtifactKind::BinaryRaw | ExtractionArtifactKind::Yaml
            ) {
                return Err(RepresentationContractError::InvalidFallbackContent);
            }
            if preferred_path.portability_key() == fallback.path.portability_key() {
                return Err(RepresentationContractError::FallbackPathCollision(
                    preferred_path.as_str().to_owned(),
                ));
            }
        }
        if working_set_bytes == 0 {
            return Err(RepresentationContractError::ZeroWorkingSet { ordinal });
        }
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.address() != Some(address))
        {
            return Err(RepresentationContractError::InvalidDiagnosticAddress { ordinal });
        }
        diagnostics.sort_unstable();
        diagnostics.dedup();
        Ok(Self {
            preferred_path,
            preferred_content,
            representation_semantics,
            fallback,
            working_set_bytes,
            diagnostics: diagnostics.into_boxed_slice(),
        })
    }

    pub(in crate::extraction) const fn preferred_kind(&self) -> ExtractionArtifactKind {
        self.preferred_content.artifact_kind()
    }

    pub(in crate::extraction) const fn preferred_path(&self) -> &ExtractionPath {
        &self.preferred_path
    }

    pub(in crate::extraction) const fn preferred_content(&self) -> &PlannedContent {
        &self.preferred_content
    }

    pub(in crate::extraction) const fn representation_semantics(&self) -> RepresentationSemantics {
        self.representation_semantics
    }

    pub(in crate::extraction) fn validate_current_semantics(
        &self,
    ) -> Result<(), RepresentationContractError> {
        self.preferred_content
            .validate_semantics(self.representation_semantics)?;
        if let Some(fallback) = self.fallback.as_ref() {
            fallback
                .content()
                .validate_semantics(fallback.representation_semantics())?;
        }
        Ok(())
    }

    pub(in crate::extraction) const fn preferred_requires_write_budget(&self) -> bool {
        self.preferred_content.requires_write_budget()
    }

    pub(in crate::extraction) fn fallback_kind(&self) -> Option<ExtractionArtifactKind> {
        self.fallback.as_ref().map(PlannedFallback::kind)
    }

    pub(in crate::extraction) fn fallback_path(&self) -> Option<&ExtractionPath> {
        self.fallback.as_ref().map(PlannedFallback::path)
    }

    pub(in crate::extraction) const fn fallback(&self) -> Option<&PlannedFallback> {
        self.fallback.as_ref()
    }

    pub(in crate::extraction) const fn working_set_bytes(&self) -> u64 {
        self.working_set_bytes
    }

    pub(in crate::extraction) const fn diagnostics(&self) -> &[ExtractionDiagnostic] {
        &self.diagnostics
    }

    pub(in crate::extraction) fn matches_output(
        &self,
        kind: ExtractionArtifactKind,
        path: &ExtractionPath,
    ) -> bool {
        (self.preferred_kind() == kind && &self.preferred_path == path)
            || self
                .fallback
                .as_ref()
                .is_some_and(|fallback| fallback.kind() == kind && &fallback.path == path)
    }

    pub(in crate::extraction) fn validate_sources(
        &self,
        sources: &[ExtractionSourceExpectation],
    ) -> Result<(), RepresentationContractError> {
        validate_content_sources(sources, &self.preferred_content)
    }

    pub(in crate::extraction) fn validate_request(
        &self,
        object_kind: ObjectKind,
        class_id: i32,
        policy: ExtractionRepresentationPolicy,
    ) -> Result<(), RepresentationContractError> {
        self.preferred_content
            .validate_request(object_kind, class_id, policy)?;
        let decoded_preferred = !matches!(
            self.preferred_kind(),
            ExtractionArtifactKind::BinaryRaw | ExtractionArtifactKind::Yaml
        );
        if policy == ExtractionRepresentationPolicy::PreferDecoded
            && decoded_preferred
            && self.fallback.is_none()
        {
            return Err(RepresentationContractError::RepresentationPolicyMismatch {
                policy,
                artifact_kind: self.preferred_kind(),
            });
        }
        if let Some(fallback) = self
            .fallback
            .as_ref()
            .filter(|_| policy != ExtractionRepresentationPolicy::PreferDecoded)
        {
            return Err(RepresentationContractError::RepresentationPolicyMismatch {
                policy,
                artifact_kind: fallback.kind(),
            });
        }
        if policy == ExtractionRepresentationPolicy::PreferDecoded
            && self.preferred_kind() == ExtractionArtifactKind::BinaryRaw
            && !self
                .diagnostics
                .iter()
                .any(|diagnostic| is_planning_fallback_reason(diagnostic.code()))
        {
            return Err(RepresentationContractError::MissingFallbackDiagnostic);
        }
        Ok(())
    }

    #[cfg(feature = "decode")]
    pub(in crate::extraction) const fn requires_stream_resolution(&self) -> bool {
        self.preferred_content.stream_source().is_some()
    }

    pub(in crate::extraction) const fn dependency_locator(&self) -> Option<&SourceLocator> {
        self.preferred_content.dependency_locator()
    }

    pub(in crate::extraction) const fn stream_source_expectation(
        &self,
    ) -> Option<&ExtractionSourceExpectation> {
        match self.preferred_content.stream_source() {
            Some(stream) => Some(stream.source()),
            None => None,
        }
    }

    #[cfg(not(feature = "decode"))]
    pub(in crate::extraction) fn preferred_extension(&self) -> &'static str {
        self.preferred_content.canonical_extension()
    }

    #[cfg(not(feature = "decode"))]
    pub(in crate::extraction) fn fallback_extension(&self) -> Option<&'static str> {
        self.fallback
            .as_ref()
            .map(|fallback| fallback.content().canonical_extension())
    }
}

const fn is_planning_fallback_reason(code: ExtractionDiagnosticCode) -> bool {
    matches!(
        code,
        ExtractionDiagnosticCode::DecodedUnavailable
            | ExtractionDiagnosticCode::FeatureUnavailable
            | ExtractionDiagnosticCode::UnsupportedClass
            | ExtractionDiagnosticCode::UnsupportedMediaEncoding
            | ExtractionDiagnosticCode::UnsupportedMediaLayout
            | ExtractionDiagnosticCode::MissingResource
            | ExtractionDiagnosticCode::UnresolvedDependency
            | ExtractionDiagnosticCode::UnresolvedSpritePPtr
            | ExtractionDiagnosticCode::SourceChanged
    )
}

fn validate_content_sources(
    sources: &[ExtractionSourceExpectation],
    content: &PlannedContent,
) -> Result<(), RepresentationContractError> {
    if let PlannedContent::SpritePng { texture, .. } = content {
        validate_source_for_address(sources, texture)?;
    }
    if let Some(stream) = content.stream_source() {
        expectation_for(sources, stream.request().owner())?;
        let actual = expectation_for(sources, stream.source().locator())?;
        if actual.fingerprint() != stream.source().fingerprint() {
            return Err(RepresentationContractError::SourceFingerprintMismatch {
                locator: stream.source().locator().clone(),
                expected: stream.source().fingerprint(),
                actual: actual.fingerprint(),
            });
        }
    }
    Ok(())
}

fn validate_source_for_address(
    sources: &[ExtractionSourceExpectation],
    address: &ObjectAddress,
) -> Result<(), RepresentationContractError> {
    let source = expectation_for(sources, address.source_locator())?;
    let expected = match address.kind() {
        ObjectKind::Binary => SourceKind::SerializedFile,
        ObjectKind::Yaml => SourceKind::Yaml,
    };
    let actual = source.fingerprint().kind();
    if actual != expected {
        return Err(RepresentationContractError::SourceKindMismatch {
            locator: source.locator().clone(),
            expected,
            actual,
        });
    }
    Ok(())
}

fn expectation_for<'source>(
    sources: &'source [ExtractionSourceExpectation],
    locator: &SourceLocator,
) -> Result<&'source ExtractionSourceExpectation, RepresentationContractError> {
    sources
        .binary_search_by(|source| source.locator().cmp(locator))
        .map(|index| &sources[index])
        .map_err(|_| RepresentationContractError::MissingSourceExpectation(locator.clone()))
}

#[derive(Debug, Error)]
pub(in crate::extraction) enum RepresentationContractError {
    #[error("{artifact_kind:?} representation is missing its implementation semantics")]
    MissingRepresentationSemantics {
        artifact_kind: ExtractionArtifactKind,
    },
    #[error(
        "representation semantics {declared:?} do not match the current implementation {expected:?}"
    )]
    RepresentationSemanticsMismatch {
        declared: RepresentationSemantics,
        expected: RepresentationSemantics,
    },
    #[error("media descriptor family is {actual:?}; representation requires {expected:?}")]
    MediaDescriptorFamilyMismatch {
        expected: MediaFamily,
        actual: MediaFamily,
    },
    #[error("artifact path {path:?} must end in the canonical .{expected} suffix")]
    ArtifactExtensionMismatch {
        path: String,
        expected: &'static str,
    },
    #[error("artifact declares kind {declared:?}, but content requires {actual:?}")]
    ArtifactKindMismatch {
        declared: ExtractionArtifactKind,
        actual: ExtractionArtifactKind,
    },
    #[error("preferred and fallback outputs collide at {0:?}")]
    FallbackPathCollision(String),
    #[error("decoded extraction fallbacks must be raw binary outputs")]
    InvalidFallbackContent,
    #[error("planned artifact {ordinal} declares a zero-byte working set")]
    ZeroWorkingSet { ordinal: u32 },
    #[error("planned artifact {ordinal} contains a diagnostic for another object")]
    InvalidDiagnosticAddress { ordinal: u32 },
    #[error("source {0:?} has no expected fingerprint")]
    MissingSourceExpectation(SourceLocator),
    #[error("source {locator:?} has kind {actual:?}; object requires {expected:?}")]
    SourceKindMismatch {
        locator: SourceLocator,
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error("source {locator:?} has fingerprint {actual}; representation requires {expected}")]
    SourceFingerprintMismatch {
        locator: SourceLocator,
        expected: unity_asset_core::SourceFingerprint,
        actual: unity_asset_core::SourceFingerprint,
    },
    #[error("{object_kind:?} object cannot use {artifact_kind:?} extraction content")]
    ObjectKindContentMismatch {
        object_kind: ObjectKind,
        artifact_kind: ExtractionArtifactKind,
    },
    #[error("class {class_id} cannot use {artifact_kind:?} extraction content")]
    ClassContentMismatch {
        class_id: i32,
        artifact_kind: ExtractionArtifactKind,
    },
    #[error("{policy:?} extraction cannot use {artifact_kind:?} content")]
    RepresentationPolicyMismatch {
        policy: ExtractionRepresentationPolicy,
        artifact_kind: ExtractionArtifactKind,
    },
    #[error("prefer-decoded raw fallback requires a deterministic planning diagnostic")]
    MissingFallbackDiagnostic,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> ExtractionPath {
        ExtractionPath::new(value).unwrap()
    }

    fn address() -> ObjectAddress {
        ObjectAddress::binary_direct(SourceLocator::path("content.assets").unwrap(), 41).unwrap()
    }

    #[test]
    fn contract_derives_output_kinds_from_normalized_content() {
        let fallback = PlannedFallback::new(path("object.raw.bin"), PlannedContent::RawBinary)
            .expect("raw binary is the only valid fallback");
        let contract = RepresentationContract::from_parts(
            0,
            &address(),
            RepresentationContractParts {
                preferred_path: path("object.txt"),
                preferred_content: PlannedContent::TextAsset,
                fallback: Some(fallback),
                working_set_bytes: 1,
                diagnostics: Vec::new(),
            },
        )
        .unwrap();

        assert_eq!(contract.preferred_kind(), ExtractionArtifactKind::Text);
        assert_eq!(
            contract.fallback_kind(),
            Some(ExtractionArtifactKind::BinaryRaw)
        );
    }

    #[test]
    fn fallback_construction_rejects_every_non_raw_representation() {
        let error = PlannedFallback::new(path("object.txt"), PlannedContent::TextAsset)
            .expect_err("decoded content must not enter normalized fallback state");

        assert!(matches!(
            error,
            RepresentationContractError::InvalidFallbackContent
        ));
    }

    #[test]
    fn representation_semantics_have_stable_canonical_ids() {
        assert_eq!(
            serde_json::to_string(&RepresentationSemantics::raw_binary()).unwrap(),
            r#"{"kind":"raw_binary","bytes":"workspace_object_raw_bytes_v1"}"#
        );
        assert_eq!(
            serde_json::to_string(&RepresentationSemantics::text_asset()).unwrap(),
            r#"{"kind":"text_asset","bytes":"type_tree_script_bytes_v1"}"#
        );
        assert_eq!(
            serde_json::to_string(&RepresentationSemantics::audio()).unwrap(),
            r#"{"kind":"audio","preparation":"prepared_standard_audio_source_v1"}"#
        );
        assert_eq!(
            serde_json::to_string(&RepresentationSemantics::texture_png()).unwrap(),
            r#"{"kind":"texture_png","pixels":"top_left_rgba8_v1","platform_transform":"serialized_file_build_target_closed_v1","encoder":"filter_none_stored_deflate_block_per_idat_rgba8_v1"}"#
        );
        assert_eq!(
            serde_json::to_string(&RepresentationSemantics::sprite_png()).unwrap(),
            r#"{"kind":"sprite_png","pixels":"top_left_rgba8_v1","platform_transform":"serialized_file_build_target_closed_v1","crop":"top_left_texture_space_v1","encoder":"filter_none_stored_deflate_block_per_idat_rgba8_v1"}"#
        );
    }

    #[test]
    fn declared_semantics_must_match_the_selected_content() {
        let error = RepresentationContract::from_declared_parts(
            0,
            &address(),
            RepresentationSemantics::raw_binary(),
            RepresentationContractParts {
                preferred_path: path("object.txt"),
                preferred_content: PlannedContent::TextAsset,
                fallback: None,
                working_set_bytes: 1,
                diagnostics: Vec::new(),
            },
        )
        .expect_err("raw-byte semantics cannot authorize TextAsset output");

        assert!(matches!(
            error,
            RepresentationContractError::RepresentationSemanticsMismatch { .. }
        ));
    }
}
