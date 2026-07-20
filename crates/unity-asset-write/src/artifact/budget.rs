use std::collections::TryReserveError;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use thiserror::Error;
use unity_asset_core::{
    AllocationSizeError, SourceFingerprint, SourceId, arc_vec_allocation_bytes,
    vec_allocation_bytes,
};

use super::payload::{ArtifactBacking, ArtifactBackingIdentity};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Resource ceilings for one prepared-artifact build budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactLimits {
    max_outputs: u64,
    max_proof_images: u64,
    max_segments: u64,
    max_publication_bytes: u64,
    max_proof_bytes: u64,
    max_generated_bytes: u64,
    max_generated_chunk_bytes: u64,
    max_metadata_bytes: u64,
    max_pinned_source_bytes: u64,
    max_retained_bytes: u64,
    max_scratch_bytes: u64,
}

impl ArtifactLimits {
    #[must_use]
    pub const fn with_max_outputs(mut self, value: u64) -> Self {
        self.max_outputs = value;
        self
    }

    #[must_use]
    pub const fn with_max_proof_images(mut self, value: u64) -> Self {
        self.max_proof_images = value;
        self
    }

    #[must_use]
    pub const fn with_max_segments(mut self, value: u64) -> Self {
        self.max_segments = value;
        self
    }

    #[must_use]
    pub const fn with_max_publication_bytes(mut self, value: u64) -> Self {
        self.max_publication_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_proof_bytes(mut self, value: u64) -> Self {
        self.max_proof_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_generated_bytes(mut self, value: u64) -> Self {
        self.max_generated_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_generated_chunk_bytes(mut self, value: u64) -> Self {
        self.max_generated_chunk_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_metadata_bytes(mut self, value: u64) -> Self {
        self.max_metadata_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_pinned_source_bytes(mut self, value: u64) -> Self {
        self.max_pinned_source_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_retained_bytes(mut self, value: u64) -> Self {
        self.max_retained_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_scratch_bytes(mut self, value: u64) -> Self {
        self.max_scratch_bytes = value;
        self
    }

    #[must_use]
    pub const fn max_outputs(self) -> u64 {
        self.max_outputs
    }

    #[must_use]
    pub const fn max_proof_images(self) -> u64 {
        self.max_proof_images
    }

    #[must_use]
    pub const fn max_segments(self) -> u64 {
        self.max_segments
    }

    #[must_use]
    pub const fn max_publication_bytes(self) -> u64 {
        self.max_publication_bytes
    }

    #[must_use]
    pub const fn max_proof_bytes(self) -> u64 {
        self.max_proof_bytes
    }

    #[must_use]
    pub const fn max_generated_bytes(self) -> u64 {
        self.max_generated_bytes
    }

    #[must_use]
    pub const fn max_generated_chunk_bytes(self) -> u64 {
        self.max_generated_chunk_bytes
    }

    #[must_use]
    pub const fn max_metadata_bytes(self) -> u64 {
        self.max_metadata_bytes
    }

    #[must_use]
    pub const fn max_pinned_source_bytes(self) -> u64 {
        self.max_pinned_source_bytes
    }

    #[must_use]
    pub const fn max_retained_bytes(self) -> u64 {
        self.max_retained_bytes
    }

    #[must_use]
    pub const fn max_scratch_bytes(self) -> u64 {
        self.max_scratch_bytes
    }

    fn validate(self) -> Result<(), ArtifactBudgetError> {
        for (resource, value) in [
            ("outputs", self.max_outputs),
            ("proof_images", self.max_proof_images),
            ("segments", self.max_segments),
            ("publication_bytes", self.max_publication_bytes),
            ("proof_bytes", self.max_proof_bytes),
            ("generated_bytes", self.max_generated_bytes),
            ("generated_chunk_bytes", self.max_generated_chunk_bytes),
            ("metadata_bytes", self.max_metadata_bytes),
            ("pinned_source_bytes", self.max_pinned_source_bytes),
            ("retained_bytes", self.max_retained_bytes),
            ("scratch_bytes", self.max_scratch_bytes),
        ] {
            if value == 0 {
                return Err(ArtifactBudgetError::InvalidLimit { resource });
            }
        }
        Ok(())
    }
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        Self {
            max_outputs: 1_000_000,
            max_proof_images: 4_000_000,
            max_segments: 4_000_000,
            max_publication_bytes: 8 * GIB,
            max_proof_bytes: 16 * GIB,
            max_generated_bytes: 2 * GIB,
            max_generated_chunk_bytes: GIB,
            max_metadata_bytes: 512 * MIB,
            max_pinned_source_bytes: 16 * GIB,
            max_retained_bytes: 20 * GIB,
            max_scratch_bytes: 2 * GIB,
        }
    }
}

/// Committed resource usage plus the observed live-scratch high-water mark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactBudgetUsage {
    outputs: u64,
    proof_images: u64,
    segments: u64,
    publication_bytes: u64,
    proof_bytes: u64,
    generated_bytes: u64,
    metadata_bytes: u64,
    pinned_source_bytes: u64,
    retained_bytes: u64,
    peak_scratch_bytes: u64,
}

impl ArtifactBudgetUsage {
    #[must_use]
    pub const fn outputs(self) -> u64 {
        self.outputs
    }

    #[must_use]
    pub const fn proof_images(self) -> u64 {
        self.proof_images
    }

    #[must_use]
    pub const fn segments(self) -> u64 {
        self.segments
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
    pub const fn peak_scratch_bytes(self) -> u64 {
        self.peak_scratch_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackingClass {
    Generated,
    Source,
}

#[derive(Debug)]
struct RetainedBacking {
    _backing: ArtifactBacking,
    allocation_bytes: u64,
    class: BackingClass,
    retained_by_proof_image: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceProof {
    fingerprint: SourceFingerprint,
    backing_identity: Option<ArtifactBackingIdentity>,
}

/// Mutable ledger shared by all artifacts produced during one prepare operation.
pub struct ArtifactBudget {
    limits: ArtifactLimits,
    usage: ArtifactBudgetUsage,
    scratch: Arc<ScratchLedger>,
}

/// Cloneable capability for charging dynamic codec allocations to one artifact transaction.
///
/// A codec may retain this capability while streaming dependency bytes. Every live lease is part
/// of the transaction's scratch high-water mark and prevents commit until it is dropped.
#[derive(Clone)]
pub(crate) struct CodecScratchBudget {
    ledger: Arc<TransactionLedger>,
}

/// One live codec allocation charged to an artifact transaction.
pub(crate) struct CodecScratchLease {
    _allocation: ScratchAllocation,
}

impl CodecScratchBudget {
    pub(crate) fn try_reserve(&self, bytes: u64) -> Result<CodecScratchLease, ArtifactBudgetError> {
        let mut allocation = ScratchAllocation::new_outstanding(Arc::clone(&self.ledger));
        if let Err(error) = allocation.reserve(bytes) {
            allocation.poison_transaction();
            return Err(error);
        }
        Ok(CodecScratchLease {
            _allocation: allocation,
        })
    }
}

impl fmt::Debug for ArtifactBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactBudget")
            .field("limits", &self.limits)
            .field("usage", &self.usage())
            .field("live_scratch_bytes", &self.scratch.live())
            .finish()
    }
}

impl ArtifactBudget {
    pub fn new(limits: ArtifactLimits) -> Result<Self, ArtifactBudgetError> {
        limits.validate()?;
        Ok(Self {
            limits,
            usage: ArtifactBudgetUsage::default(),
            scratch: Arc::new(ScratchLedger::new(limits.max_scratch_bytes)),
        })
    }

    #[must_use]
    pub const fn limits(&self) -> ArtifactLimits {
        self.limits
    }

    #[must_use]
    pub fn usage(&self) -> ArtifactBudgetUsage {
        ArtifactBudgetUsage {
            peak_scratch_bytes: self.scratch.peak(),
            ..self.usage
        }
    }

    /// Returns only atomically committed retained usage, excluding construction scratch peaks.
    #[must_use]
    pub const fn committed_usage(&self) -> ArtifactBudgetUsage {
        self.usage
    }

    #[cfg(test)]
    pub(crate) fn live_scratch_bytes(&self) -> u64 {
        self.scratch.live()
    }

    pub(crate) fn transaction(&mut self) -> ArtifactBudgetTransaction<'_> {
        let transaction = Arc::new(TransactionLedger::new(Arc::clone(&self.scratch)));
        ArtifactBudgetTransaction {
            budget: self,
            pending: ArtifactBudgetUsage::default(),
            source_proofs: FallibleTable::new(Arc::clone(&transaction)),
            retained_backings: FallibleTable::new(Arc::clone(&transaction)),
            transaction,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ScratchLedger {
    limit: u64,
    live: AtomicU64,
    peak: AtomicU64,
}

impl ScratchLedger {
    const fn new(limit: u64) -> Self {
        Self {
            limit,
            live: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }

    pub(crate) fn reserve(&self, bytes: u64) -> Result<(), ArtifactBudgetError> {
        let mut live = self.live.load(Ordering::Relaxed);
        loop {
            let requested =
                live.checked_add(bytes)
                    .ok_or(ArtifactBudgetError::ArithmeticOverflow {
                        resource: "scratch_bytes",
                    })?;
            if requested > self.limit {
                return Err(ArtifactBudgetError::Exceeded {
                    resource: "scratch_bytes",
                    requested,
                    limit: self.limit,
                });
            }
            match self.live.compare_exchange_weak(
                live,
                requested,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.peak.fetch_max(requested, Ordering::Relaxed);
                    return Ok(());
                }
                Err(actual) => live = actual,
            }
        }
    }

    pub(crate) fn release(&self, bytes: u64) {
        let previous = self.live.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(previous >= bytes, "scratch ledger underflow");
    }

    pub(crate) fn live(&self) -> u64 {
        self.live.load(Ordering::Relaxed)
    }

    pub(crate) fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct TransactionLedger {
    scratch: Arc<ScratchLedger>,
    outstanding: AtomicU64,
    poisoned: AtomicBool,
}

impl TransactionLedger {
    fn new(scratch: Arc<ScratchLedger>) -> Self {
        Self {
            scratch,
            outstanding: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
        }
    }

    fn reserve(&self, bytes: u64) -> Result<(), ArtifactBudgetError> {
        self.scratch.reserve(bytes)
    }

    fn release(&self, bytes: u64) {
        self.scratch.release(bytes);
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> u64 {
        self.scratch.live()
    }

    fn open_handle(&self) {
        self.outstanding.fetch_add(1, Ordering::Relaxed);
    }

    fn close_handle(&self) {
        let previous = self.outstanding.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "transaction handle ledger underflow");
    }

    fn outstanding(&self) -> u64 {
        self.outstanding.load(Ordering::Acquire)
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }
}

pub(crate) struct ArtifactBudgetTransaction<'budget> {
    budget: &'budget mut ArtifactBudget,
    pending: ArtifactBudgetUsage,
    source_proofs: FallibleTable<SourceId, SourceProof>,
    retained_backings: FallibleTable<ArtifactBackingIdentity, RetainedBacking>,
    transaction: Arc<TransactionLedger>,
}

pub(crate) struct GeneratedBackingReservation<'transaction> {
    retained_backings: &'transaction mut FallibleTable<ArtifactBackingIdentity, RetainedBacking>,
    transaction: Arc<TransactionLedger>,
    allocation_bytes: u64,
    finalized: bool,
}

impl GeneratedBackingReservation<'_> {
    pub(crate) const fn allocation_bytes(&self) -> u64 {
        self.allocation_bytes
    }

    pub(crate) fn finalize(mut self, backing: ArtifactBacking) {
        debug_assert_eq!(backing.allocation_bytes().ok(), Some(self.allocation_bytes));
        let previous = self.retained_backings.insert_reserved(
            backing.identity(),
            RetainedBacking {
                _backing: backing,
                allocation_bytes: self.allocation_bytes,
                class: BackingClass::Generated,
                retained_by_proof_image: false,
            },
        );
        assert!(
            previous.is_none(),
            "a generated Arc<Vec<u8>> allocation identity must be unique"
        );
        self.finalized = true;
    }
}

impl Drop for GeneratedBackingReservation<'_> {
    fn drop(&mut self) {
        if !self.finalized {
            self.transaction.poison();
        }
    }
}

impl ArtifactBudgetTransaction<'_> {
    pub(crate) fn reserve_output_declaration(
        &mut self,
        name_bytes: u64,
    ) -> Result<(), ArtifactBudgetError> {
        let mut next = self.pending;
        reserve(
            "outputs",
            self.budget.usage.outputs,
            &mut next.outputs,
            1,
            self.budget.limits.max_outputs,
        )?;
        reserve(
            "metadata_bytes",
            self.budget.usage.metadata_bytes,
            &mut next.metadata_bytes,
            name_bytes,
            self.budget.limits.max_metadata_bytes,
        )?;
        reserve_retained(
            &self.budget.usage,
            &mut next,
            name_bytes,
            self.budget.limits.max_retained_bytes,
        )?;
        self.pending = next;
        Ok(())
    }

    pub(crate) fn reserve_proof_images(&mut self, amount: u64) -> Result<(), ArtifactBudgetError> {
        reserve(
            "proof_images",
            self.budget.usage.proof_images,
            &mut self.pending.proof_images,
            amount,
            self.budget.limits.max_proof_images,
        )
    }

    pub(crate) fn reserve_segments(&mut self, amount: u64) -> Result<(), ArtifactBudgetError> {
        reserve(
            "segments",
            self.budget.usage.segments,
            &mut self.pending.segments,
            amount,
            self.budget.limits.max_segments,
        )
    }

    pub(crate) fn reserve_publication_bytes(
        &mut self,
        amount: u64,
    ) -> Result<(), ArtifactBudgetError> {
        reserve(
            "publication_bytes",
            self.budget.usage.publication_bytes,
            &mut self.pending.publication_bytes,
            amount,
            self.budget.limits.max_publication_bytes,
        )
    }

    pub(crate) fn reserve_proof_bytes(&mut self, amount: u64) -> Result<(), ArtifactBudgetError> {
        reserve(
            "proof_bytes",
            self.budget.usage.proof_bytes,
            &mut self.pending.proof_bytes,
            amount,
            self.budget.limits.max_proof_bytes,
        )
    }

    pub(crate) fn validate_generated_chunk_len(
        &self,
        amount: u64,
    ) -> Result<(), ArtifactBudgetError> {
        if amount > self.budget.limits.max_generated_chunk_bytes {
            return Err(ArtifactBudgetError::Exceeded {
                resource: "generated_chunk_bytes",
                requested: amount,
                limit: self.budget.limits.max_generated_chunk_bytes,
            });
        }
        Ok(())
    }

    pub(crate) fn preflight_generated(
        &mut self,
        length: usize,
        capacity: usize,
    ) -> Result<GeneratedBackingReservation<'_>, ArtifactBudgetError> {
        if self.transaction.is_poisoned() {
            return Err(ArtifactBudgetError::PoisonedTransaction);
        }
        if length > capacity {
            return Err(ArtifactBudgetError::InvalidGeneratedCapacity { length, capacity });
        }
        self.validate_generated_chunk_len(usize_to_u64(length, "generated_chunk_bytes")?)?;
        let allocation_bytes = arc_vec_allocation_bytes::<u8>(capacity)?;
        let mut next = self.pending;
        reserve(
            "generated_bytes",
            self.budget.usage.generated_bytes,
            &mut next.generated_bytes,
            allocation_bytes,
            self.budget.limits.max_generated_bytes,
        )?;
        reserve_retained(
            &self.budget.usage,
            &mut next,
            allocation_bytes,
            self.budget.limits.max_retained_bytes,
        )?;
        self.retained_backings
            .reserve_for_insert("retained backing registry")?;
        self.pending = next;
        Ok(GeneratedBackingReservation {
            retained_backings: &mut self.retained_backings,
            transaction: Arc::clone(&self.transaction),
            allocation_bytes,
            finalized: false,
        })
    }

    pub(crate) fn reserve_generated_backing(
        &mut self,
        backing: &ArtifactBacking,
    ) -> Result<bool, ArtifactBudgetError> {
        self.validate_generated_chunk_len(usize_to_u64(backing.len(), "generated_chunk_bytes")?)?;
        let identity = backing.identity();
        if let Some(retained) = self.retained_backings.get_mut(&identity) {
            retained.retained_by_proof_image = true;
            return Ok(false);
        }
        let allocation_bytes = backing.allocation_bytes()?;
        let mut next = self.pending;
        reserve(
            "generated_bytes",
            self.budget.usage.generated_bytes,
            &mut next.generated_bytes,
            allocation_bytes,
            self.budget.limits.max_generated_bytes,
        )?;
        reserve_retained(
            &self.budget.usage,
            &mut next,
            allocation_bytes,
            self.budget.limits.max_retained_bytes,
        )?;

        self.retained_backings
            .reserve_for_insert("retained backing registry")?;
        let previous = self.retained_backings.insert_reserved(
            identity,
            RetainedBacking {
                _backing: backing.clone(),
                allocation_bytes,
                class: BackingClass::Generated,
                retained_by_proof_image: true,
            },
        );
        debug_assert!(previous.is_none());
        self.pending = next;
        Ok(true)
    }

    pub(crate) fn reserve_metadata_bytes(
        &mut self,
        amount: u64,
    ) -> Result<(), ArtifactBudgetError> {
        let mut next = self.pending;
        reserve(
            "metadata_bytes",
            self.budget.usage.metadata_bytes,
            &mut next.metadata_bytes,
            amount,
            self.budget.limits.max_metadata_bytes,
        )?;
        reserve_retained(
            &self.budget.usage,
            &mut next,
            amount,
            self.budget.limits.max_retained_bytes,
        )?;
        self.pending = next;
        Ok(())
    }

    pub(crate) fn reserve_source_backing(
        &mut self,
        source: SourceId,
        fingerprint: SourceFingerprint,
        backing: &ArtifactBacking,
    ) -> Result<bool, ArtifactBudgetError> {
        let identity = backing.identity();
        let source_is_new = match self.source_proofs.get(&source).copied() {
            Some(existing) => {
                if existing.fingerprint != fingerprint {
                    return Err(ArtifactBudgetError::ConflictingSourceFingerprint {
                        source_id: Box::new(source),
                        first: existing.fingerprint.digest(),
                        second: fingerprint.digest(),
                    });
                }
                if existing
                    .backing_identity
                    .is_some_and(|existing| existing != identity)
                {
                    return Err(ArtifactBudgetError::ConflictingSourceBacking {
                        source_id: source,
                    });
                }
                false
            }
            None => true,
        };

        let existing_backing = self
            .retained_backings
            .get(&identity)
            .map(|retained| (retained.class, retained.allocation_bytes));
        let allocation_bytes = backing.allocation_bytes()?;
        let mut next = self.pending;
        match existing_backing {
            Some((BackingClass::Generated, existing_bytes)) => {
                next.generated_bytes = next.generated_bytes.checked_sub(existing_bytes).ok_or(
                    ArtifactBudgetError::ArithmeticOverflow {
                        resource: "generated_bytes",
                    },
                )?;
                reserve(
                    "pinned_source_bytes",
                    self.budget.usage.pinned_source_bytes,
                    &mut next.pinned_source_bytes,
                    allocation_bytes,
                    self.budget.limits.max_pinned_source_bytes,
                )?;
            }
            Some((BackingClass::Source, _)) => {}
            None => {
                reserve(
                    "pinned_source_bytes",
                    self.budget.usage.pinned_source_bytes,
                    &mut next.pinned_source_bytes,
                    allocation_bytes,
                    self.budget.limits.max_pinned_source_bytes,
                )?;
                reserve_retained(
                    &self.budget.usage,
                    &mut next,
                    allocation_bytes,
                    self.budget.limits.max_retained_bytes,
                )?;
            }
        }

        if source_is_new {
            self.source_proofs
                .reserve_for_insert("source proof registry")?;
        }
        if existing_backing.is_none() {
            self.retained_backings
                .reserve_for_insert("retained backing registry")?;
        }

        if source_is_new {
            let previous = self.source_proofs.insert_reserved(
                source,
                SourceProof {
                    fingerprint,
                    backing_identity: Some(identity),
                },
            );
            debug_assert!(previous.is_none());
        } else {
            let proof = self.source_proofs.get_mut(&source).ok_or(
                ArtifactBudgetError::InternalRegistryInvariant {
                    resource: "source proof registry",
                },
            )?;
            proof.backing_identity = Some(identity);
        }
        match self.retained_backings.get_mut(&identity) {
            Some(retained) => {
                retained.class = BackingClass::Source;
                retained.retained_by_proof_image = true;
            }
            None => {
                let previous = self.retained_backings.insert_reserved(
                    identity,
                    RetainedBacking {
                        _backing: backing.clone(),
                        allocation_bytes,
                        class: BackingClass::Source,
                        retained_by_proof_image: true,
                    },
                );
                debug_assert!(previous.is_none());
            }
        }
        self.pending = next;
        Ok(existing_backing.is_none())
    }

    pub(crate) fn reserve_source_proof(
        &mut self,
        source: SourceId,
        fingerprint: SourceFingerprint,
    ) -> Result<bool, ArtifactBudgetError> {
        if let Some(existing) = self.source_proofs.get(&source).copied() {
            if existing.fingerprint != fingerprint {
                return Err(ArtifactBudgetError::ConflictingSourceFingerprint {
                    source_id: Box::new(source),
                    first: existing.fingerprint.digest(),
                    second: fingerprint.digest(),
                });
            }
            return Ok(false);
        }

        self.source_proofs
            .reserve_for_insert("source proof registry")?;
        let previous = self.source_proofs.insert_reserved(
            source,
            SourceProof {
                fingerprint,
                backing_identity: None,
            },
        );
        debug_assert!(previous.is_none());
        Ok(true)
    }

    pub(crate) fn grow_retained_vec<T>(
        &mut self,
        values: &mut Vec<T>,
        additional: usize,
        resource: &'static str,
    ) -> Result<(), ArtifactBudgetError> {
        let required = required_capacity(values, additional, resource)?;
        if required <= values.capacity() {
            return Ok(());
        }
        let target = geometric_capacity(values.capacity(), required, resource)?;
        let old_bytes = vec_allocation_bytes::<T>(values.capacity())?;
        let planned_bytes = vec_allocation_bytes::<T>(target)?;
        self.transaction.reserve(planned_bytes)?;
        if let Err(source) = values.try_reserve_exact(target - values.len()) {
            self.transaction.release(planned_bytes);
            return Err(ArtifactBudgetError::Allocation {
                resource,
                requested: target,
                source,
            });
        }
        let actual_bytes = match vec_allocation_bytes::<T>(values.capacity()) {
            Ok(bytes) => bytes,
            Err(error) => {
                drop(mem::take(values));
                self.transaction.release(planned_bytes);
                return Err(error.into());
            }
        };
        if let Err(error) =
            reserve_scratch_supplement(&self.transaction, actual_bytes, planned_bytes)
        {
            drop(mem::take(values));
            self.transaction.release(planned_bytes);
            return Err(error);
        }
        let added_metadata = match actual_bytes.checked_sub(old_bytes) {
            Some(bytes) => bytes,
            None => {
                drop(mem::take(values));
                self.transaction.release(actual_bytes);
                return Err(ArtifactBudgetError::ArithmeticOverflow {
                    resource: "metadata_bytes",
                });
            }
        };
        if let Err(error) = self.reserve_metadata_bytes(added_metadata) {
            drop(mem::take(values));
            self.transaction.release(actual_bytes);
            return Err(error);
        }
        self.transaction.release(actual_bytes);
        Ok(())
    }

    pub(crate) fn scratch_ledger(&self) -> Arc<TransactionLedger> {
        Arc::clone(&self.transaction)
    }

    pub(crate) fn scratch_allocation(&self) -> ScratchAllocation {
        ScratchAllocation::new_outstanding(Arc::clone(&self.transaction))
    }

    pub(crate) fn codec_scratch_budget(&self) -> CodecScratchBudget {
        CodecScratchBudget {
            ledger: Arc::clone(&self.transaction),
        }
    }

    pub(crate) const fn max_generated_chunk_bytes(&self) -> u64 {
        self.budget.limits.max_generated_chunk_bytes
    }

    pub(crate) fn owns_scratch_ledger(&self, ledger: &Arc<TransactionLedger>) -> bool {
        Arc::ptr_eq(&self.transaction, ledger)
    }

    pub(crate) fn transaction_is_poisoned(&self) -> bool {
        self.transaction.is_poisoned()
    }

    #[must_use]
    pub(crate) const fn pending_usage(&self) -> ArtifactBudgetUsage {
        self.pending
    }

    pub(crate) fn commit(self) -> Result<(), ArtifactBudgetError> {
        if self.transaction.is_poisoned() {
            return Err(ArtifactBudgetError::PoisonedTransaction);
        }
        let outstanding = self.transaction.outstanding();
        if outstanding != 0 {
            return Err(ArtifactBudgetError::OutstandingTransactionReservations { outstanding });
        }
        let unretained = self
            .retained_backings
            .slots
            .iter()
            .flatten()
            .filter(|(_, backing)| !backing.retained_by_proof_image)
            .count();
        if unretained != 0 {
            return Err(ArtifactBudgetError::UnretainedGeneratedBackings {
                count: usize_to_u64(unretained, "unretained_generated_backings")?,
            });
        }
        let committed = add_usage(self.budget.usage, self.pending)?;
        self.budget.usage = committed;
        Ok(())
    }
}

pub(crate) struct FallibleTable<K, V> {
    slots: Vec<Option<(K, V)>>,
    len: usize,
    hash_builder: RandomState,
    scratch: ScratchAllocation,
}

impl<K, V> FallibleTable<K, V>
where
    K: Eq + Hash,
{
    pub(crate) fn new(ledger: Arc<TransactionLedger>) -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
            hash_builder: RandomState::new(),
            scratch: ScratchAllocation::new_internal(ledger),
        }
    }

    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.slot_index(key)
            .and_then(|index| self.slots[index].as_ref().map(|(_, value)| value))
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let index = self.slot_index(key)?;
        self.slots[index].as_mut().map(|(_, value)| value)
    }

    pub(crate) fn reserve_for_insert(
        &mut self,
        resource: &'static str,
    ) -> Result<(), ArtifactBudgetError> {
        let required = self
            .len
            .checked_add(1)
            .ok_or(ArtifactBudgetError::ArithmeticOverflow { resource })?;
        if !self.slots.is_empty() && required <= self.slots.len() / 2 {
            return Ok(());
        }
        let next_slots = if self.slots.is_empty() {
            8
        } else {
            self.slots
                .len()
                .checked_mul(2)
                .ok_or(ArtifactBudgetError::ArithmeticOverflow { resource })?
        };
        self.grow(next_slots, resource)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Result<Option<V>, ArtifactBudgetError> {
        let slot_count = self.slots.len();
        if slot_count == 0 {
            return Err(ArtifactBudgetError::InternalTableFull {
                resource: "fallible table",
            });
        }
        let mut index = table_index(&self.hash_builder, &key, slot_count);
        for _ in 0..slot_count {
            match &mut self.slots[index] {
                Some((existing_key, existing_value)) if existing_key == &key => {
                    return Ok(Some(mem::replace(existing_value, value)));
                }
                Some(_) => index = (index + 1) & (slot_count - 1),
                slot @ None => {
                    *slot = Some((key, value));
                    self.len += 1;
                    return Ok(None);
                }
            }
        }
        Err(ArtifactBudgetError::InternalTableFull {
            resource: "fallible table",
        })
    }

    pub(crate) fn insert_reserved(&mut self, key: K, value: V) -> Option<V> {
        match self.insert(key, value) {
            Ok(previous) => previous,
            Err(error) => panic!("reserved open-address table insertion failed: {error}"),
        }
    }

    fn slot_index(&self, key: &K) -> Option<usize> {
        let slot_count = self.slots.len();
        if slot_count == 0 {
            return None;
        }
        let mut index = table_index(&self.hash_builder, key, slot_count);
        for _ in 0..slot_count {
            match &self.slots[index] {
                Some((existing_key, _)) if existing_key == key => return Some(index),
                Some(_) => index = (index + 1) & (slot_count - 1),
                None => return None,
            }
        }
        None
    }

    fn grow(
        &mut self,
        slot_count: usize,
        resource: &'static str,
    ) -> Result<(), ArtifactBudgetError> {
        debug_assert!(slot_count.is_power_of_two());
        let mut new_scratch = ScratchAllocation::new_internal(Arc::clone(self.scratch.ledger()));
        let mut new_slots = Vec::new();
        new_scratch.grow_vec(&mut new_slots, slot_count, resource)?;
        new_slots.resize_with(slot_count, || None);

        let old_slots = mem::take(&mut self.slots);
        for (key, value) in old_slots.into_iter().flatten() {
            insert_table_entry(&self.hash_builder, &mut new_slots, key, value);
        }
        self.slots = new_slots;
        let old_scratch = mem::replace(&mut self.scratch, new_scratch);
        drop(old_scratch);
        Ok(())
    }
}

fn table_index<K: Hash>(hash_builder: &RandomState, key: &K, slot_count: usize) -> usize {
    debug_assert!(slot_count.is_power_of_two());
    hash_builder.hash_one(key) as usize & (slot_count - 1)
}

fn insert_table_entry<K, V>(
    hash_builder: &RandomState,
    slots: &mut [Option<(K, V)>],
    key: K,
    value: V,
) where
    K: Eq + Hash,
{
    let mut index = table_index(hash_builder, &key, slots.len());
    loop {
        if slots[index].is_none() {
            slots[index] = Some((key, value));
            return;
        }
        index = (index + 1) & (slots.len() - 1);
    }
}

pub(crate) struct ScratchAllocation {
    ledger: Arc<TransactionLedger>,
    bytes: u64,
    outstanding_handle: bool,
}

impl ScratchAllocation {
    fn new_internal(ledger: Arc<TransactionLedger>) -> Self {
        Self {
            ledger,
            bytes: 0,
            outstanding_handle: false,
        }
    }

    fn new_outstanding(ledger: Arc<TransactionLedger>) -> Self {
        ledger.open_handle();
        Self {
            ledger,
            bytes: 0,
            outstanding_handle: true,
        }
    }

    pub(crate) fn grow_vec<T>(
        &mut self,
        values: &mut Vec<T>,
        additional: usize,
        resource: &'static str,
    ) -> Result<(), ArtifactBudgetError> {
        ensure_scratch_vec_capacity(&self.ledger, values, additional, &mut self.bytes, resource)
    }

    pub(crate) fn reserve(&mut self, bytes: u64) -> Result<(), ArtifactBudgetError> {
        let next =
            self.bytes
                .checked_add(bytes)
                .ok_or(ArtifactBudgetError::ArithmeticOverflow {
                    resource: "scratch_bytes",
                })?;
        self.ledger.reserve(bytes)?;
        self.bytes = next;
        Ok(())
    }

    pub(crate) fn validate_for_retention(
        &self,
        retained_bytes: u64,
    ) -> Result<(), ArtifactBudgetError> {
        if self.bytes != retained_bytes {
            return Err(ArtifactBudgetError::ScratchPromotionMismatch {
                scratch_bytes: self.bytes,
                retained_bytes,
            });
        }
        Ok(())
    }

    pub(crate) fn release_for_retention(&mut self) {
        self.ledger.release(self.bytes);
        self.bytes = 0;
        if self.outstanding_handle {
            self.ledger.close_handle();
            self.outstanding_handle = false;
        }
    }

    pub(crate) fn ledger(&self) -> &Arc<TransactionLedger> {
        &self.ledger
    }

    pub(crate) fn poison_transaction(&self) {
        self.ledger.poison();
    }
}

impl Drop for ScratchAllocation {
    fn drop(&mut self) {
        self.ledger.release(self.bytes);
        if self.outstanding_handle {
            self.ledger.close_handle();
        }
    }
}

fn ensure_scratch_vec_capacity<T>(
    ledger: &TransactionLedger,
    values: &mut Vec<T>,
    additional: usize,
    accounted_bytes: &mut u64,
    resource: &'static str,
) -> Result<(), ArtifactBudgetError> {
    let required = required_capacity(values, additional, resource)?;
    if required <= values.capacity() {
        return Ok(());
    }
    let target = geometric_capacity(values.capacity(), required, resource)?;
    let planned_bytes = vec_allocation_bytes::<T>(target)?;
    ledger.reserve(planned_bytes)?;
    if let Err(source) = values.try_reserve_exact(target - values.len()) {
        ledger.release(planned_bytes);
        return Err(ArtifactBudgetError::Allocation {
            resource,
            requested: target,
            source,
        });
    }
    let actual_bytes = match vec_allocation_bytes::<T>(values.capacity()) {
        Ok(bytes) => bytes,
        Err(error) => {
            drop(mem::take(values));
            ledger.release(*accounted_bytes);
            ledger.release(planned_bytes);
            *accounted_bytes = 0;
            return Err(error.into());
        }
    };
    if let Err(error) = reserve_scratch_supplement(ledger, actual_bytes, planned_bytes) {
        drop(mem::take(values));
        ledger.release(*accounted_bytes);
        ledger.release(planned_bytes);
        *accounted_bytes = 0;
        return Err(error);
    }
    ledger.release(*accounted_bytes);
    *accounted_bytes = actual_bytes;
    Ok(())
}

fn reserve_scratch_supplement(
    ledger: &TransactionLedger,
    actual_bytes: u64,
    planned_bytes: u64,
) -> Result<(), ArtifactBudgetError> {
    let supplement =
        actual_bytes
            .checked_sub(planned_bytes)
            .ok_or(ArtifactBudgetError::ArithmeticOverflow {
                resource: "scratch_bytes",
            })?;
    ledger.reserve(supplement)
}

fn required_capacity<T>(
    values: &[T],
    additional: usize,
    resource: &'static str,
) -> Result<usize, ArtifactBudgetError> {
    values
        .len()
        .checked_add(additional)
        .ok_or(ArtifactBudgetError::ArithmeticOverflow { resource })
}

fn geometric_capacity(
    current: usize,
    required: usize,
    resource: &'static str,
) -> Result<usize, ArtifactBudgetError> {
    const MIN_CAPACITY: usize = 4;

    let doubled = current
        .max(MIN_CAPACITY)
        .checked_mul(2)
        .ok_or(ArtifactBudgetError::ArithmeticOverflow { resource })?;
    Ok(doubled.max(required))
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, ArtifactBudgetError> {
    u64::try_from(value).map_err(|_| ArtifactBudgetError::ArithmeticOverflow { resource })
}

fn reserve(
    resource: &'static str,
    committed: u64,
    pending: &mut u64,
    amount: u64,
    limit: u64,
) -> Result<(), ArtifactBudgetError> {
    let next_pending = pending
        .checked_add(amount)
        .ok_or(ArtifactBudgetError::ArithmeticOverflow { resource })?;
    let requested = committed
        .checked_add(next_pending)
        .ok_or(ArtifactBudgetError::ArithmeticOverflow { resource })?;
    if requested > limit {
        return Err(ArtifactBudgetError::Exceeded {
            resource,
            requested,
            limit,
        });
    }
    *pending = next_pending;
    Ok(())
}

fn reserve_retained(
    committed: &ArtifactBudgetUsage,
    pending: &mut ArtifactBudgetUsage,
    amount: u64,
    limit: u64,
) -> Result<(), ArtifactBudgetError> {
    reserve(
        "retained_bytes",
        committed.retained_bytes,
        &mut pending.retained_bytes,
        amount,
        limit,
    )
}

fn add_usage(
    committed: ArtifactBudgetUsage,
    pending: ArtifactBudgetUsage,
) -> Result<ArtifactBudgetUsage, ArtifactBudgetError> {
    Ok(ArtifactBudgetUsage {
        outputs: add_usage_value(committed.outputs, pending.outputs, "outputs")?,
        proof_images: add_usage_value(
            committed.proof_images,
            pending.proof_images,
            "proof_images",
        )?,
        segments: add_usage_value(committed.segments, pending.segments, "segments")?,
        publication_bytes: add_usage_value(
            committed.publication_bytes,
            pending.publication_bytes,
            "publication_bytes",
        )?,
        proof_bytes: add_usage_value(committed.proof_bytes, pending.proof_bytes, "proof_bytes")?,
        generated_bytes: add_usage_value(
            committed.generated_bytes,
            pending.generated_bytes,
            "generated_bytes",
        )?,
        metadata_bytes: add_usage_value(
            committed.metadata_bytes,
            pending.metadata_bytes,
            "metadata_bytes",
        )?,
        pinned_source_bytes: add_usage_value(
            committed.pinned_source_bytes,
            pending.pinned_source_bytes,
            "pinned_source_bytes",
        )?,
        retained_bytes: add_usage_value(
            committed.retained_bytes,
            pending.retained_bytes,
            "retained_bytes",
        )?,
        peak_scratch_bytes: committed.peak_scratch_bytes.max(pending.peak_scratch_bytes),
    })
}

fn add_usage_value(
    committed: u64,
    pending: u64,
    resource: &'static str,
) -> Result<u64, ArtifactBudgetError> {
    committed
        .checked_add(pending)
        .ok_or(ArtifactBudgetError::ArithmeticOverflow { resource })
}

#[derive(Debug, Error)]
pub enum ArtifactBudgetError {
    #[error(transparent)]
    AllocationSize(#[from] AllocationSizeError),
    #[error("artifact limit for {resource} must be nonzero")]
    InvalidLimit { resource: &'static str },
    #[error("artifact budget arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("generated buffer length {length} exceeds its allocation capacity {capacity}")]
    InvalidGeneratedCapacity { length: usize, capacity: usize },
    #[error("artifact budget exceeded for {resource}: requested {requested}, limit {limit}")]
    Exceeded {
        resource: &'static str,
        requested: u64,
        limit: u64,
    },
    #[error("failed to reserve {requested} entries for {resource}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("system allocation failed for {resource}: requested {requested} bytes")]
    SystemAllocationFailed {
        resource: &'static str,
        requested: u64,
    },
    #[error("internal open-address table for {resource} has no vacant slot")]
    InternalTableFull { resource: &'static str },
    #[error("internal artifact budget registry invariant failed for {resource}")]
    InternalRegistryInvariant { resource: &'static str },
    #[error(
        "scratch promotion retained {retained_bytes} bytes from a {scratch_bytes}-byte allocation"
    )]
    ScratchPromotionMismatch {
        scratch_bytes: u64,
        retained_bytes: u64,
    },
    #[error("source {source_id:?} has conflicting digests {first} and {second}")]
    ConflictingSourceFingerprint {
        source_id: Box<SourceId>,
        first: unity_asset_core::DigestV1,
        second: unity_asset_core::DigestV1,
    },
    #[error("source {source_id:?} is associated with multiple backing allocations")]
    ConflictingSourceBacking { source_id: SourceId },
    #[error("artifact budget transaction is poisoned by an incomplete promotion")]
    PoisonedTransaction,
    #[error(
        "artifact budget transaction has {outstanding} outstanding scratch or generated reservations"
    )]
    OutstandingTransactionReservations { outstanding: u64 },
    #[error(
        "artifact budget transaction has {count} generated backing allocations not retained by a proof image"
    )]
    UnretainedGeneratedBackings { count: u64 },
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash, Hasher};

    use unity_asset_core::{
        SourceKind, VerifiedSourceImage, WorkspaceId, arc_slice_allocation_bytes,
        vec_allocation_bytes,
    };

    use super::*;
    use crate::artifact::payload::ArtifactBacking;

    fn source_id() -> SourceId {
        SourceId::new(
            WorkspaceId::from_u128(9).unwrap(),
            SourceKind::SerializedFile,
            3,
        )
        .unwrap()
    }

    fn other_source_id() -> SourceId {
        SourceId::new(
            WorkspaceId::from_u128(9).unwrap(),
            SourceKind::SerializedFile,
            4,
        )
        .unwrap()
    }

    #[test]
    fn same_source_and_fingerprint_cannot_switch_backing_allocation() {
        let first =
            VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::from(b"same".as_slice()));
        let second =
            VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::from(b"same".as_slice()));
        let first_backing = ArtifactBacking::shared_slice(Arc::clone(first.backing()));
        let second_backing = ArtifactBacking::shared_slice(Arc::clone(second.backing()));
        assert_ne!(first_backing.identity(), second_backing.identity());

        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        transaction
            .reserve_source_backing(source_id(), first.fingerprint(), &first_backing)
            .unwrap();
        let before = transaction.pending_usage();
        let error = transaction
            .reserve_source_backing(source_id(), second.fingerprint(), &second_backing)
            .unwrap_err();

        assert!(matches!(
            error,
            ArtifactBudgetError::ConflictingSourceBacking { .. }
        ));
        assert_eq!(transaction.pending_usage(), before);
    }

    #[test]
    fn source_identity_can_be_registered_before_a_nonempty_backing_is_bound() {
        let verified = VerifiedSourceImage::verify(
            SourceKind::SerializedFile,
            Arc::<[u8]>::from(b"bound later".as_slice()),
        );
        let backing = ArtifactBacking::shared_slice(Arc::clone(verified.backing()));
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();

        assert!(
            transaction
                .reserve_source_proof(source_id(), verified.fingerprint())
                .unwrap()
        );
        assert_eq!(transaction.pending_usage(), ArtifactBudgetUsage::default());
        assert!(
            transaction
                .reserve_source_backing(source_id(), verified.fingerprint(), &backing)
                .unwrap()
        );
        assert_eq!(
            transaction.pending_usage().pinned_source_bytes(),
            backing.allocation_bytes().unwrap()
        );
    }

    #[test]
    fn different_sources_can_share_one_backing_allocation() {
        let verified =
            VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::from(b"same".as_slice()));
        let backing = ArtifactBacking::shared_slice(Arc::clone(verified.backing()));
        let allocation = arc_slice_allocation_bytes::<u8>(4).unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();

        transaction
            .reserve_source_backing(source_id(), verified.fingerprint(), &backing)
            .unwrap();
        transaction
            .reserve_source_backing(other_source_id(), verified.fingerprint(), &backing)
            .unwrap();

        assert_eq!(
            transaction.pending_usage().pinned_source_bytes(),
            allocation
        );
        assert_eq!(transaction.pending_usage().retained_bytes(), allocation);
    }

    #[test]
    fn shared_generated_and_source_backing_is_retained_once_with_source_priority() {
        let shared: Arc<[u8]> = Arc::from(b"shared".as_slice());
        let verified = VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::clone(&shared));
        let backing = ArtifactBacking::shared_slice(shared);
        let allocation = backing.allocation_bytes().unwrap();

        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        transaction.reserve_generated_backing(&backing).unwrap();
        transaction
            .reserve_source_backing(source_id(), verified.fingerprint(), &backing)
            .unwrap();

        let usage = transaction.pending_usage();
        assert_eq!(usage.generated_bytes(), 0);
        assert_eq!(usage.pinned_source_bytes(), allocation);
        assert_eq!(usage.retained_bytes(), allocation);
    }

    #[test]
    fn conflicting_source_fingerprint_does_not_partially_charge_backing() {
        let first =
            VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::from(b"first".as_slice()));
        let conflicting = VerifiedSourceImage::verify(
            SourceKind::SerializedFile,
            Arc::from(b"conflicting".as_slice()),
        );
        let first_backing = ArtifactBacking::shared_slice(Arc::clone(first.backing()));
        let conflicting_backing = ArtifactBacking::shared_slice(Arc::clone(conflicting.backing()));

        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        transaction
            .reserve_source_backing(source_id(), first.fingerprint(), &first_backing)
            .unwrap();
        let before = transaction.pending_usage();
        let error = transaction
            .reserve_source_backing(source_id(), conflicting.fingerprint(), &conflicting_backing)
            .unwrap_err();

        assert!(matches!(
            error,
            ArtifactBudgetError::ConflictingSourceFingerprint { .. }
        ));
        assert_eq!(transaction.pending_usage(), before);
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CollidingKey(u32);

    impl Hash for CollidingKey {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0_u8.hash(state);
        }
    }

    #[test]
    fn fallible_table_resolves_collisions_and_accounts_reallocation_peak() {
        let scratch = Arc::new(ScratchLedger::new(u64::MAX));
        let transaction = Arc::new(TransactionLedger::new(Arc::clone(&scratch)));
        let mut table = FallibleTable::new(transaction);

        for value in 0..256 {
            table.reserve_for_insert("collision table").unwrap();
            assert_eq!(table.insert(CollidingKey(value), value).unwrap(), None);
        }

        for value in 0..256 {
            assert_eq!(table.get(&CollidingKey(value)), Some(&value));
        }
        assert!(table.slots.len().is_power_of_two());
        assert!(table.len <= table.slots.len() / 2);

        let current =
            vec_allocation_bytes::<Option<(CollidingKey, u32)>>(table.slots.capacity()).unwrap();
        let previous =
            vec_allocation_bytes::<Option<(CollidingKey, u32)>>(table.slots.len() / 2).unwrap();
        assert_eq!(scratch.live(), current);
        assert!(scratch.peak() >= current + previous);

        drop(table);
        assert_eq!(scratch.live(), 0);
    }

    #[test]
    fn failed_table_growth_leaves_no_allocation_or_entry() {
        let scratch = Arc::new(ScratchLedger::new(1));
        let transaction = Arc::new(TransactionLedger::new(Arc::clone(&scratch)));
        let mut table = FallibleTable::<u64, u64>::new(transaction);

        let error = table.reserve_for_insert("small table").unwrap_err();

        assert!(matches!(error, ArtifactBudgetError::Exceeded { .. }));
        assert!(table.slots.is_empty());
        assert_eq!(table.len, 0);
        assert_eq!(scratch.live(), 0);
    }

    #[test]
    fn transaction_registry_is_scratch_and_released_after_commit() {
        let verified = VerifiedSourceImage::verify(
            SourceKind::SerializedFile,
            Arc::from(b"source".as_slice()),
        );
        let backing = ArtifactBacking::shared_slice(Arc::clone(verified.backing()));
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();

        {
            let mut transaction = budget.transaction();
            transaction
                .reserve_source_backing(source_id(), verified.fingerprint(), &backing)
                .unwrap();
            assert_eq!(transaction.pending_usage().metadata_bytes(), 0);
            assert!(transaction.budget.scratch.live() > 0);
            transaction.commit().unwrap();
        }

        assert_eq!(budget.scratch.live(), 0);
        assert_eq!(budget.usage().metadata_bytes(), 0);
    }

    #[test]
    fn abandoned_generated_preflight_poisons_the_transaction() {
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();

        let reservation = transaction.preflight_generated(1, 8).unwrap();
        drop(reservation);

        assert!(matches!(
            transaction.commit(),
            Err(ArtifactBudgetError::PoisonedTransaction)
        ));
        assert_eq!(budget.committed_usage(), ArtifactBudgetUsage::default());
        assert_eq!(budget.scratch.live(), 0);
    }
}
