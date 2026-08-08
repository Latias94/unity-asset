//! Budgeted, immutable output graphs produced by Unity artifact encoders.
//!
//! Public names are declared and sealed before encoding. Encoders then append independently
//! inspected proof images leaf-to-root, bind output roots, and atomically commit the complete
//! reachable graph.

use thiserror::Error;
use unity_asset_core::{DigestBuildError, DigestV1};

mod batch;
mod budget;
mod codec;
mod footprint;
mod format;
mod image;
mod name;
mod payload;

pub use batch::{
    ArtifactBatch, ArtifactBatchDeclaration, ArtifactBuildError, ArtifactBuildFailurePhase,
    ArtifactHandle, OutputSlot, PreparedArtifact, PreparedArtifactSet, PreparedOutput,
    PreparedOutputIter, YamlArtifactWriter,
};
pub use budget::{ArtifactBudget, ArtifactBudgetError, ArtifactBudgetUsage, ArtifactLimits};
pub(crate) use budget::{CodecScratchBudget, CodecScratchLease};
pub(crate) use codec::encode_brotli;
pub use footprint::{
    ArtifactBuildCounters, ArtifactFootprint, ArtifactSetFootprint, ArtifactSourceDependency,
};
pub(crate) use format::{PreparedArtifactFormat, VerbatimSourceInspection};
pub use format::{
    PreparedArtifactFormatProof, PreparedArtifactKind, PreparedArtifactSourceCompatibility,
    PreparedArtifactSourceCompatibilityError, PreparedYamlProof, ResourceLayoutDigest,
    StreamedResourceExtentInspection, StreamedResourceInspection, YamlInspection,
};
pub use image::ArtifactReader;
pub use name::{ArtifactNameError, LogicalArtifactName};
pub use payload::{ArtifactPayload, ArtifactPayloadError, ArtifactPayloadProvenance};

pub(crate) use image::ImageBuilder;

/// Receipt from one sequential write-and-verify pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactStreamReceipt {
    pub(crate) bytes_written: u64,
    pub(crate) digest: DigestV1,
}

impl ArtifactStreamReceipt {
    #[must_use]
    pub const fn bytes_written(self) -> u64 {
        self.bytes_written
    }

    #[must_use]
    pub const fn digest(self) -> DigestV1 {
        self.digest
    }
}

#[derive(Debug, Error)]
pub enum ArtifactStreamError {
    #[error("failed to stream a prepared artifact: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
    #[error("prepared artifact digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        expected: DigestV1,
        actual: DigestV1,
    },
}

#[cfg(test)]
mod tests;
