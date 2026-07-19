use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use thiserror::Error;

use crate::{SourceFingerprint, SourceKind};

/// Immutable source bytes whose fingerprint was derived from the retained backing.
///
/// Construction hashes the complete backing exactly once. Consumers can therefore
/// trust the kind/fingerprint/bytes relationship without rescanning the source.
#[derive(Clone)]
pub struct VerifiedSourceImage {
    kind: SourceKind,
    bytes: Arc<[u8]>,
    fingerprint: SourceFingerprint,
}

/// Opaque proof that a verified source may move to an equivalent canonical allocation.
///
/// The previous allocation remains alive until this proof is consumed, allowing downstream
/// parsed views to validate their exact slice identity without comparing or hashing bytes again.
#[must_use = "consume the rebinding proof to obtain the canonical verified image"]
pub struct VerifiedSourceRebinding {
    previous: Arc<[u8]>,
    rebound: VerifiedSourceImage,
}

impl VerifiedSourceImage {
    #[must_use]
    pub fn verify(kind: SourceKind, bytes: Arc<[u8]>) -> Self {
        let fingerprint = SourceFingerprint::from_bytes(kind, &bytes);
        Self {
            kind,
            bytes,
            fingerprint,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn backing(&self) -> &Arc<[u8]> {
        &self.bytes
    }

    /// Rebinds this proof to an equivalent canonical allocation without rehashing.
    ///
    /// Content-addressed stores use this after a digest lookup. Byte equality is
    /// still checked so a digest collision cannot change the proven image.
    pub fn rebind_equivalent(self, canonical: Arc<[u8]>) -> Result<Self, VerifiedSourceImageError> {
        self.rebind_equivalent_with_proof(canonical)
            .map(VerifiedSourceRebinding::into_image)
    }

    /// Proves an equivalent canonical allocation while retaining the previous allocation identity.
    ///
    /// Byte equality is checked exactly once when the allocations differ. Consumers of the proof
    /// can subsequently rebind parsed views using pointer-and-range identity only.
    pub fn rebind_equivalent_with_proof(
        self,
        canonical: Arc<[u8]>,
    ) -> Result<VerifiedSourceRebinding, VerifiedSourceImageError> {
        if !Arc::ptr_eq(&self.bytes, &canonical) && self.bytes.as_ref() != canonical.as_ref() {
            return Err(VerifiedSourceImageError::NonEquivalentBacking {
                fingerprint: self.fingerprint,
            });
        }
        let previous = self.bytes;
        let rebound = Self {
            bytes: canonical,
            kind: self.kind,
            fingerprint: self.fingerprint,
        };
        Ok(VerifiedSourceRebinding { previous, rebound })
    }
}

impl VerifiedSourceRebinding {
    /// Verifies that `candidate` is the complete slice of the previous source allocation.
    pub fn ensure_previous_backing(
        &self,
        expected_kind: SourceKind,
        candidate: Option<&Arc<[u8]>>,
        range: Range<usize>,
    ) -> Result<(), VerifiedSourceImageError> {
        if self.rebound.kind != expected_kind {
            return Err(VerifiedSourceImageError::SourceKindMismatch {
                expected: expected_kind,
                actual: self.rebound.kind,
            });
        }
        let is_previous_complete_backing = candidate.is_some_and(|candidate| {
            Arc::ptr_eq(candidate, &self.previous) && range == (0..self.previous.len())
        });
        if !is_previous_complete_backing {
            return Err(VerifiedSourceImageError::PreviousBackingMismatch {
                fingerprint: self.rebound.fingerprint,
            });
        }
        Ok(())
    }

    /// Returns the equivalent canonical allocation selected for the rebound source image.
    #[must_use]
    pub fn canonical_backing(&self) -> &Arc<[u8]> {
        self.rebound.backing()
    }

    /// Consumes the proof and returns the verified image bound to the canonical allocation.
    #[must_use]
    pub fn into_image(self) -> VerifiedSourceImage {
        self.rebound
    }
}

impl fmt::Debug for VerifiedSourceRebinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedSourceRebinding")
            .field("fingerprint", &self.rebound.fingerprint)
            .field("previous_bytes", &self.previous.len())
            .field("canonical_bytes", &self.rebound.bytes.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for VerifiedSourceImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedSourceImage")
            .field("kind", &self.kind)
            .field("fingerprint", &self.fingerprint)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VerifiedSourceImageError {
    #[error("canonical backing does not match verified source {fingerprint}")]
    NonEquivalentBacking { fingerprint: SourceFingerprint },
    #[error("verified source kind {actual:?} does not match required kind {expected:?}")]
    SourceKindMismatch {
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error("parsed source does not retain the previous backing for {fingerprint}")]
    PreviousBackingMismatch { fingerprint: SourceFingerprint },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_binds_kind_digest_and_backing() {
        let backing: Arc<[u8]> = Arc::from(b"verified".as_slice());
        let image = VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::clone(&backing));

        assert_eq!(image.kind(), SourceKind::SerializedFile);
        assert_eq!(
            image.fingerprint(),
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, b"verified")
        );
        assert!(Arc::ptr_eq(image.backing(), &backing));
    }

    #[test]
    fn equivalent_rebind_uses_canonical_allocation_without_rehashing() {
        let original: Arc<[u8]> = Arc::from(b"shared".as_slice());
        let canonical: Arc<[u8]> = Arc::from(b"shared".as_slice());
        let image = VerifiedSourceImage::verify(SourceKind::Archive, original);
        let rebound = image
            .rebind_equivalent(Arc::clone(&canonical))
            .expect("equal bytes can use the canonical allocation");

        assert!(Arc::ptr_eq(rebound.backing(), &canonical));
        assert_eq!(
            rebound.fingerprint(),
            SourceFingerprint::from_bytes(SourceKind::Archive, b"shared")
        );
    }

    #[test]
    fn rebind_rejects_digest_collision_candidates_with_different_bytes() {
        let image =
            VerifiedSourceImage::verify(SourceKind::Archive, Arc::from(b"expected".as_slice()));
        let error = image
            .rebind_equivalent(Arc::from(b"different".as_slice()))
            .expect_err("different bytes cannot reuse a digest slot");

        assert!(matches!(
            error,
            VerifiedSourceImageError::NonEquivalentBacking { .. }
        ));
    }

    #[test]
    fn rebinding_proof_cannot_be_minted_for_different_bytes() {
        let image =
            VerifiedSourceImage::verify(SourceKind::Archive, Arc::from(b"expected".as_slice()));
        let error = image
            .rebind_equivalent_with_proof(Arc::from(b"different".as_slice()))
            .expect_err("a rebinding proof must not be minted for different bytes");

        assert!(matches!(
            error,
            VerifiedSourceImageError::NonEquivalentBacking { .. }
        ));
    }

    #[test]
    fn rebinding_proof_tracks_both_allocations_and_releases_the_previous_one() {
        let previous: Arc<[u8]> = Arc::from(b"shared".as_slice());
        let canonical: Arc<[u8]> = Arc::from(b"shared".as_slice());
        let image = VerifiedSourceImage::verify(SourceKind::Archive, Arc::clone(&previous));
        let proof = image
            .rebind_equivalent_with_proof(Arc::clone(&canonical))
            .expect("equal bytes produce a rebinding proof");

        proof
            .ensure_previous_backing(SourceKind::Archive, Some(&previous), 0..previous.len())
            .expect("the complete previous allocation is accepted");
        assert!(matches!(
            proof.ensure_previous_backing(SourceKind::Archive, Some(&previous), 1..previous.len()),
            Err(VerifiedSourceImageError::PreviousBackingMismatch { .. })
        ));
        assert!(matches!(
            proof.ensure_previous_backing(
                SourceKind::Archive,
                Some(&canonical),
                0..canonical.len()
            ),
            Err(VerifiedSourceImageError::PreviousBackingMismatch { .. })
        ));
        assert!(matches!(
            proof.ensure_previous_backing(
                SourceKind::SerializedFile,
                Some(&previous),
                0..previous.len()
            ),
            Err(VerifiedSourceImageError::SourceKindMismatch {
                expected: SourceKind::SerializedFile,
                actual: SourceKind::Archive,
            })
        ));
        assert!(Arc::ptr_eq(proof.canonical_backing(), &canonical));

        let rebound = proof.into_image();
        assert!(Arc::ptr_eq(rebound.backing(), &canonical));
        assert_eq!(Arc::strong_count(&previous), 1);
    }
}
