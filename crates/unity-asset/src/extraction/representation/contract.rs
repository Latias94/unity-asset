//! Validated representation lifecycle persisted by an extraction plan.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use unity_asset_core::{ObjectAddress, ObjectKind, SourceKind, SourceLocator};
use unity_asset_decode::descriptor::{MediaDescriptor, MediaFamily};

use super::super::contract::{
    ExtractionArtifactKind, ExtractionDiagnostic, ExtractionPath, ExtractionSourceExpectation,
};
use crate::workspace::StreamedResourceRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::extraction) struct PlannedFallback {
    path: ExtractionPath,
    content: PlannedContent,
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
        Ok(Self { path, content })
    }

    pub(in crate::extraction) fn from_declared_parts(
        kind: ExtractionArtifactKind,
        path: ExtractionPath,
        content: PlannedContent,
    ) -> Result<Self, RepresentationContractError> {
        content.validate_declared_kind(kind)?;
        Self::new(path, content)
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
}

/// Inert representation selected during extraction planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(in crate::extraction) enum PlannedContent {
    RawBinary,
    Yaml,
    TextAsset,
    Audio {
        stream: Option<StreamedResourceRequest>,
        descriptor: MediaDescriptor,
    },
    TexturePng {
        stream: Option<StreamedResourceRequest>,
        descriptor: MediaDescriptor,
    },
    SpritePng {
        texture: ObjectAddress,
        texture_stream: Option<StreamedResourceRequest>,
        descriptor: MediaDescriptor,
    },
}

impl PlannedContent {
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

    pub(in crate::extraction) const fn stream_request(&self) -> Option<&StreamedResourceRequest> {
        match self {
            Self::Audio { stream, .. } | Self::TexturePng { stream, .. } => stream.as_ref(),
            Self::SpritePng { texture_stream, .. } => texture_stream.as_ref(),
            Self::RawBinary | Self::Yaml | Self::TextAsset => None,
        }
    }

    const fn requires_write_budget(&self) -> bool {
        matches!(self, Self::Yaml)
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
        let RepresentationContractParts {
            preferred_path,
            preferred_content,
            fallback,
            working_set_bytes,
            mut diagnostics,
        } = parts;
        preferred_content.validate()?;
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

    #[cfg(feature = "decode")]
    pub(in crate::extraction) fn requires_stream_resolution(&self) -> bool {
        self.preferred_content.stream_request().is_some()
    }
}

fn validate_content_sources(
    sources: &[ExtractionSourceExpectation],
    content: &PlannedContent,
) -> Result<(), RepresentationContractError> {
    if let PlannedContent::SpritePng { texture, .. } = content {
        validate_source_for_address(sources, texture)?;
    }
    if let Some(stream) = content.stream_request() {
        expectation_for(sources, stream.owner())?;
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
}
