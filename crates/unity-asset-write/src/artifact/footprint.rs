use unity_asset_core::{SourceFingerprint, SourceId};

use super::ArtifactBuildError;

/// One immutable source image referenced without copying into an artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactSourceDependency {
    pub(crate) source: SourceId,
    pub(crate) fingerprint: SourceFingerprint,
    pub(crate) referenced_bytes: u64,
}

impl ArtifactSourceDependency {
    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub const fn referenced_bytes(&self) -> u64 {
        self.referenced_bytes
    }
}

/// Logical and retained costs attributable to one independently proved image.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactFootprint {
    pub(crate) proof_bytes: u64,
    pub(crate) retained_bytes: u64,
    pub(crate) referenced_source_bytes: u64,
    pub(crate) generated_bytes: u64,
    pub(crate) metadata_bytes: u64,
    pub(crate) pinned_source_bytes: u64,
    pub(crate) inspection_bytes: u64,
    pub(crate) segments: u64,
}

impl ArtifactFootprint {
    #[must_use]
    pub const fn proof_bytes(self) -> u64 {
        self.proof_bytes
    }

    #[must_use]
    pub const fn retained_bytes(self) -> u64 {
        self.retained_bytes
    }

    #[must_use]
    pub const fn referenced_source_bytes(self) -> u64 {
        self.referenced_source_bytes
    }

    #[must_use]
    pub const fn generated_bytes(self) -> u64 {
        self.generated_bytes
    }

    #[must_use]
    pub const fn metadata_bytes(self) -> u64 {
        self.metadata_bytes
    }

    #[must_use]
    pub const fn pinned_source_bytes(self) -> u64 {
        self.pinned_source_bytes
    }

    #[must_use]
    pub const fn inspection_bytes(self) -> u64 {
        self.inspection_bytes
    }

    #[must_use]
    pub const fn segments(self) -> u64 {
        self.segments
    }
}

/// Committed resource footprint of a complete prepared-artifact graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactSetFootprint {
    pub(crate) outputs: u64,
    pub(crate) proof_images: u64,
    pub(crate) publication_bytes: u64,
    pub(crate) proof_bytes: u64,
    pub(crate) generated_bytes: u64,
    pub(crate) metadata_bytes: u64,
    pub(crate) pinned_source_bytes: u64,
    pub(crate) retained_bytes: u64,
    pub(crate) referenced_source_bytes: u64,
    pub(crate) segments: u64,
}

impl ArtifactSetFootprint {
    #[must_use]
    pub const fn outputs(self) -> u64 {
        self.outputs
    }

    #[must_use]
    pub const fn proof_images(self) -> u64 {
        self.proof_images
    }

    #[must_use]
    pub const fn publication_bytes(self) -> u64 {
        self.publication_bytes
    }

    #[must_use]
    pub const fn proof_bytes(self) -> u64 {
        self.proof_bytes
    }

    #[must_use]
    pub const fn generated_bytes(self) -> u64 {
        self.generated_bytes
    }

    #[must_use]
    pub const fn metadata_bytes(self) -> u64 {
        self.metadata_bytes
    }

    #[must_use]
    pub const fn pinned_source_bytes(self) -> u64 {
        self.pinned_source_bytes
    }

    #[must_use]
    pub const fn retained_bytes(self) -> u64 {
        self.retained_bytes
    }

    #[must_use]
    pub const fn referenced_source_bytes(self) -> u64 {
        self.referenced_source_bytes
    }

    #[must_use]
    pub const fn segments(self) -> u64 {
        self.segments
    }
}

/// Observable pass counts for one artifact build or an aggregate set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactBuildCounters {
    pub(crate) source_ranges: u64,
    pub(crate) generated_chunks: u64,
    pub(crate) digest_passes: u64,
    pub(crate) digest_reuses: u64,
    pub(crate) validation_passes: u64,
}

impl ArtifactBuildCounters {
    #[must_use]
    pub const fn source_ranges(self) -> u64 {
        self.source_ranges
    }

    #[must_use]
    pub const fn generated_chunks(self) -> u64 {
        self.generated_chunks
    }

    #[must_use]
    pub const fn digest_passes(self) -> u64 {
        self.digest_passes
    }

    #[must_use]
    pub const fn digest_reuses(self) -> u64 {
        self.digest_reuses
    }

    #[must_use]
    pub const fn validation_passes(self) -> u64 {
        self.validation_passes
    }

    pub(crate) fn checked_add(self, other: Self) -> Result<Self, ArtifactBuildError> {
        Ok(Self {
            source_ranges: checked_add(self.source_ranges, other.source_ranges, "source_ranges")?,
            generated_chunks: checked_add(
                self.generated_chunks,
                other.generated_chunks,
                "generated_chunks",
            )?,
            digest_passes: checked_add(self.digest_passes, other.digest_passes, "digest_passes")?,
            digest_reuses: checked_add(self.digest_reuses, other.digest_reuses, "digest_reuses")?,
            validation_passes: checked_add(
                self.validation_passes,
                other.validation_passes,
                "validation_passes",
            )?,
        })
    }
}

fn checked_add(left: u64, right: u64, resource: &'static str) -> Result<u64, ArtifactBuildError> {
    left.checked_add(right)
        .ok_or(ArtifactBuildError::ArithmeticOverflow { resource })
}
