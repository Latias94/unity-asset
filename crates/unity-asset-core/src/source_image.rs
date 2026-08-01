use std::fmt;
use std::ops::Deref;
use std::ops::Range;
use std::sync::Arc;

use thiserror::Error;

use crate::{
    AssetLoadBudget, BudgetError, SourceFingerprint, SourceKind, arc_slice_allocation_bytes,
    budget::AssetLoadBudgetDomain,
};

/// Immutable source bytes whose shared allocation has already been charged to one load budget.
///
/// Cloning this value only shares the existing allocation. Consumers that retain the backing can
/// therefore transfer this proof within the same budget domain instead of charging the same
/// `Arc<[u8]>` allocation again. A different budget cannot consume the proof.
#[derive(Clone)]
pub struct BudgetedSourceBytes {
    bytes: Arc<[u8]>,
    domain: Arc<AssetLoadBudgetDomain>,
}

impl BudgetedSourceBytes {
    /// Promotes caller-owned bytes into a shared backing after charging the Arc allocation.
    pub fn from_vec(bytes: Vec<u8>, budget: &mut AssetLoadBudget) -> Result<Self, BudgetError> {
        let allocation = source_arc_allocation(bytes.len())?;
        budget.consume_bytes(allocation)?;
        Ok(Self {
            bytes: Arc::from(bytes.into_boxed_slice()),
            domain: budget.domain(),
        })
    }

    /// Accounts an existing shared backing before it enters a budgeted ownership domain.
    pub fn from_arc(bytes: Arc<[u8]>, budget: &mut AssetLoadBudget) -> Result<Self, BudgetError> {
        budget.consume_bytes(source_arc_allocation(bytes.len())?)?;
        Ok(Self {
            bytes,
            domain: budget.domain(),
        })
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrows the shared backing after validating the owning budget domain.
    pub fn backing(&self, budget: &AssetLoadBudget) -> Result<&Arc<[u8]>, BudgetError> {
        self.validate_budget(budget)?;
        Ok(&self.bytes)
    }

    /// Clones the shared backing after validating the owning budget domain.
    pub fn clone_backing(&self, budget: &AssetLoadBudget) -> Result<Arc<[u8]>, BudgetError> {
        self.backing(budget).map(Arc::clone)
    }

    /// Consumes the proof after validating the owning budget domain.
    pub fn into_backing(self, budget: &AssetLoadBudget) -> Result<Arc<[u8]>, BudgetError> {
        self.validate_budget(budget)?;
        Ok(self.bytes)
    }

    /// Verifies that this proof was minted by `budget`.
    pub fn validate_budget(&self, budget: &AssetLoadBudget) -> Result<(), BudgetError> {
        if !budget.belongs_to_domain(&self.domain) {
            return Err(BudgetError::DomainMismatch {
                resource: "source bytes",
            });
        }
        Ok(())
    }

    /// Hashes the retained backing once while preserving its budget-domain proof.
    #[must_use]
    pub fn verify(self, kind: SourceKind) -> BudgetedVerifiedSourceImage {
        BudgetedVerifiedSourceImage {
            image: VerifiedSourceImage::verify(kind, self.bytes),
            domain: self.domain,
        }
    }
}

impl PartialEq for BudgetedSourceBytes {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for BudgetedSourceBytes {}

impl AsRef<[u8]> for BudgetedSourceBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for BudgetedSourceBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

impl fmt::Debug for BudgetedSourceBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BudgetedSourceBytes")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

fn source_arc_allocation(length: usize) -> Result<u64, BudgetError> {
    arc_slice_allocation_bytes::<u8>(length).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "budgeted_source_bytes",
    })
}

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

/// Verified immutable source bytes whose retained allocation belongs to one load-budget domain.
#[derive(Clone)]
pub struct BudgetedVerifiedSourceImage {
    image: VerifiedSourceImage,
    domain: Arc<AssetLoadBudgetDomain>,
}

impl BudgetedVerifiedSourceImage {
    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        self.image.kind()
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.image.fingerprint()
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.image.as_bytes()
    }

    /// Borrows the verified backing after checking its budget domain.
    pub fn backing(&self, budget: &AssetLoadBudget) -> Result<&Arc<[u8]>, BudgetError> {
        self.validate_budget(budget)?;
        Ok(self.image.backing())
    }

    /// Clones the verified backing after checking its budget domain.
    pub fn clone_backing(&self, budget: &AssetLoadBudget) -> Result<Arc<[u8]>, BudgetError> {
        self.backing(budget).map(Arc::clone)
    }

    /// Consumes the budget proof and returns the already-verified image without rehashing.
    pub fn into_image(self, budget: &AssetLoadBudget) -> Result<VerifiedSourceImage, BudgetError> {
        self.validate_budget(budget)?;
        Ok(self.image)
    }

    /// Verifies that the retained allocation belongs to `budget`.
    pub fn validate_budget(&self, budget: &AssetLoadBudget) -> Result<(), BudgetError> {
        if !budget.belongs_to_domain(&self.domain) {
            return Err(BudgetError::DomainMismatch {
                resource: "verified source image",
            });
        }
        Ok(())
    }
}

impl fmt::Debug for BudgetedVerifiedSourceImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BudgetedVerifiedSourceImage")
            .field("kind", &self.kind())
            .field("bytes", &self.as_bytes().len())
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
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
    use crate::AssetLoadLimits;

    #[test]
    fn budgeted_source_bytes_charge_once_and_clone_the_backing() {
        let expected = source_arc_allocation(6).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: expected,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let bytes = BudgetedSourceBytes::from_vec(b"shared".to_vec(), &mut budget).unwrap();
        let cloned = bytes.clone();

        assert_eq!(budget.usage().bytes, expected);
        assert!(Arc::ptr_eq(
            bytes.backing(&budget).unwrap(),
            cloned.backing(&budget).unwrap()
        ));
        assert_eq!(bytes, cloned);
    }

    #[test]
    fn budgeted_source_bytes_reject_a_different_budget_domain() {
        let mut first = AssetLoadBudget::default();
        let bytes = BudgetedSourceBytes::from_vec(b"shared".to_vec(), &mut first).unwrap();
        let cloned = bytes.clone();
        let second = AssetLoadBudget::default();

        assert!(matches!(
            cloned.into_backing(&second),
            Err(BudgetError::DomainMismatch {
                resource: "source bytes"
            })
        ));
        assert!(bytes.into_backing(&first).is_ok());
    }

    #[test]
    fn budgeted_verification_preserves_the_domain_and_verified_backing() {
        let mut first = AssetLoadBudget::default();
        let image = BudgetedSourceBytes::from_vec(b"verified".to_vec(), &mut first)
            .unwrap()
            .verify(SourceKind::SerializedFile);
        let second = AssetLoadBudget::default();

        assert_eq!(image.kind(), SourceKind::SerializedFile);
        assert_eq!(
            image.fingerprint(),
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, b"verified")
        );
        assert!(matches!(
            image.clone().into_image(&second),
            Err(BudgetError::DomainMismatch {
                resource: "verified source image"
            })
        ));

        let backing = Arc::clone(image.backing(&first).unwrap());
        let verified = image.into_image(&first).unwrap();
        assert!(Arc::ptr_eq(verified.backing(), &backing));
    }

    #[test]
    fn budgeted_source_bytes_equality_ignores_budget_domain() {
        let mut first = AssetLoadBudget::default();
        let mut second = AssetLoadBudget::default();
        let left = BudgetedSourceBytes::from_vec(b"same".to_vec(), &mut first).unwrap();
        let right = BudgetedSourceBytes::from_vec(b"same".to_vec(), &mut second).unwrap();

        assert_eq!(left, right);
    }

    #[test]
    fn budgeted_source_bytes_reject_before_arc_promotion() {
        let required = source_arc_allocation(6).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = BudgetedSourceBytes::from_vec(b"shared".to_vec(), &mut budget).unwrap_err();

        assert!(matches!(
            error,
            BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            } if limit == required - 1 && requested == required
        ));
    }

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
