use std::fmt;
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
        if !Arc::ptr_eq(&self.bytes, &canonical) && self.bytes.as_ref() != canonical.as_ref() {
            return Err(VerifiedSourceImageError::NonEquivalentBacking {
                fingerprint: self.fingerprint,
            });
        }
        Ok(Self {
            bytes: canonical,
            ..self
        })
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
}
