//! Canonical prepared-artifact encoding for Unity bundle containers.

mod artifact_writer;

pub use artifact_writer::{
    BundleArtifactEntry, BundleArtifactError, BundleArtifactMember, BundleWriter,
};
