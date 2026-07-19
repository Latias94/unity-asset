use std::fmt;
use std::io::{self, Seek, SeekFrom, Write};
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::ByteSegment;
use unity_asset_core::{
    AllocationSizeError, DigestV1, SourceFingerprint, SourceId, VerifiedSourceImage,
    arc_slice_allocation_bytes, arc_value_allocation_bytes, arc_vec_allocation_bytes,
};

use super::budget::{ArtifactBudgetError, ArtifactBudgetTransaction, ScratchAllocation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ArtifactBackingIdentity {
    SharedSlice(usize),
    GeneratedVec(usize),
}

#[derive(Clone)]
pub(crate) enum ArtifactBacking {
    SharedSlice(Arc<[u8]>),
    GeneratedVec(Arc<Vec<u8>>),
}

impl ArtifactBacking {
    pub(crate) fn shared_slice(bytes: Arc<[u8]>) -> Self {
        Self::SharedSlice(bytes)
    }

    pub(crate) fn generated_vec(bytes: Arc<Vec<u8>>) -> Self {
        Self::GeneratedVec(bytes)
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::SharedSlice(bytes) => bytes,
            Self::GeneratedVec(bytes) => bytes,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub(crate) fn identity(&self) -> ArtifactBackingIdentity {
        match self {
            Self::SharedSlice(bytes) => {
                ArtifactBackingIdentity::SharedSlice(Arc::as_ptr(bytes).cast::<u8>() as usize)
            }
            Self::GeneratedVec(bytes) => {
                ArtifactBackingIdentity::GeneratedVec(Arc::as_ptr(bytes) as usize)
            }
        }
    }

    pub(crate) fn allocation_bytes(&self) -> Result<u64, AllocationSizeError> {
        match self {
            Self::SharedSlice(bytes) => arc_slice_allocation_bytes::<u8>(bytes.len()),
            Self::GeneratedVec(bytes) => arc_vec_allocation_bytes::<u8>(bytes.capacity()),
        }
    }

    pub(crate) fn segment(
        &self,
        logical_start: u64,
        range: Range<usize>,
    ) -> unity_asset_binary::Result<ByteSegment> {
        match self {
            Self::SharedSlice(bytes) => {
                ByteSegment::from_arc_range(logical_start, Arc::clone(bytes), range)
            }
            Self::GeneratedVec(bytes) => {
                ByteSegment::from_arc_vec_range(logical_start, Arc::clone(bytes), range)
            }
        }
    }

    #[cfg(test)]
    fn as_shared_slice_arc(&self) -> Option<&Arc<[u8]>> {
        match self {
            Self::SharedSlice(bytes) => Some(bytes),
            Self::GeneratedVec(_) => None,
        }
    }
}

impl fmt::Debug for ArtifactBacking {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactBacking")
            .field("identity", &self.identity())
            .field("length", &self.len())
            .finish_non_exhaustive()
    }
}

/// Immutable provenance of bytes offered to a format encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactPayloadProvenance {
    Source {
        source_id: SourceId,
        fingerprint: SourceFingerprint,
    },
    Generated,
}

/// Immutable, digest-bound input bytes for a prepared-artifact encoder.
#[derive(Clone)]
pub struct ArtifactPayload {
    backing: ArtifactBacking,
    len: u64,
    digest_hint: Option<DigestV1>,
    provenance: ArtifactPayloadProvenance,
}

impl ArtifactPayload {
    pub fn source_backed(
        source_id: SourceId,
        image: VerifiedSourceImage,
    ) -> Result<Self, ArtifactPayloadError> {
        if source_id.kind() != image.kind() {
            return Err(ArtifactPayloadError::SourceKindMismatch {
                source_id,
                image_kind: image.kind(),
            });
        }
        let fingerprint = image.fingerprint();
        let bytes = Arc::clone(image.backing());
        Ok(Self {
            len: payload_len(bytes.len())?,
            backing: ArtifactBacking::shared_slice(bytes),
            digest_hint: Some(fingerprint.digest()),
            provenance: ArtifactPayloadProvenance::Source {
                source_id,
                fingerprint,
            },
        })
    }

    pub(crate) fn from_generated_vec(bytes: Arc<Vec<u8>>) -> Result<Self, ArtifactPayloadError> {
        Ok(Self {
            len: payload_len(bytes.len())?,
            digest_hint: None,
            backing: ArtifactBacking::generated_vec(bytes),
            provenance: ArtifactPayloadProvenance::Generated,
        })
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub const fn digest(&self) -> Option<DigestV1> {
        self.digest_hint
    }

    #[must_use]
    pub const fn provenance(&self) -> ArtifactPayloadProvenance {
        self.provenance
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.backing.as_slice()
    }

    pub(crate) const fn backing(&self) -> &ArtifactBacking {
        &self.backing
    }

    pub(crate) fn shares_shared_backing(&self, other: &Arc<[u8]>) -> bool {
        match &self.backing {
            ArtifactBacking::SharedSlice(bytes) => Arc::ptr_eq(bytes, other),
            ArtifactBacking::GeneratedVec(_) => false,
        }
    }
}

impl fmt::Debug for ArtifactPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactPayload")
            .field("len", &self.len)
            .field("digest_hint", &self.digest_hint)
            .field("provenance", &self.provenance)
            .finish_non_exhaustive()
    }
}

pub(crate) struct GeneratedChunkWriter {
    bytes: Vec<u8>,
    position: usize,
    max_chunk_bytes: u64,
    scratch: ScratchAllocation,
    failed: bool,
    poison_transaction_on_error: bool,
}

impl GeneratedChunkWriter {
    pub(crate) fn new(transaction: &ArtifactBudgetTransaction<'_>) -> Self {
        Self {
            bytes: Vec::new(),
            position: 0,
            max_chunk_bytes: transaction.max_generated_chunk_bytes(),
            scratch: transaction.scratch_allocation(),
            failed: false,
            poison_transaction_on_error: false,
        }
    }

    pub(crate) fn new_for_encoder(transaction: &ArtifactBudgetTransaction<'_>) -> Self {
        Self {
            poison_transaction_on_error: true,
            ..Self::new(transaction)
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Exposes the already-reserved generated buffer for bounded in-place codecs.
    ///
    /// Callers must size the writer before requesting the slice. This keeps codec scratch and
    /// output ownership inside the artifact budget instead of allowing an untracked temporary
    /// allocation.
    pub(crate) fn as_mut_slice(&mut self) -> Result<&mut [u8], ArtifactPayloadError> {
        self.ensure_active()?;
        Ok(self.bytes.as_mut_slice())
    }

    pub(crate) fn extend_from_slice(&mut self, bytes: &[u8]) -> Result<(), ArtifactPayloadError> {
        self.ensure_active()?;
        let original_position = self.position;
        self.position = self.bytes.len();
        if let Err(error) = self.write_at_position(bytes) {
            self.position = original_position;
            self.mark_failed();
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn resize_zero(&mut self, new_len: usize) -> Result<(), ArtifactPayloadError> {
        self.ensure_active()?;
        if let Err(error) = self.resize_zero_inner(new_len) {
            self.mark_failed();
            return Err(error);
        }
        Ok(())
    }

    fn resize_zero_inner(&mut self, new_len: usize) -> Result<(), ArtifactPayloadError> {
        self.validate_len(new_len)?;
        if new_len > self.bytes.len() {
            let additional = new_len - self.bytes.len();
            self.scratch
                .grow_vec(&mut self.bytes, additional, "generated chunk bytes")?;
            self.bytes.resize(new_len, 0);
        } else {
            self.bytes.truncate(new_len);
            self.position = self.position.min(new_len);
        }
        Ok(())
    }

    pub(crate) fn finish(
        mut self,
        transaction: &mut ArtifactBudgetTransaction<'_>,
    ) -> Result<ArtifactPayload, ArtifactPayloadError> {
        self.ensure_active()?;
        let result = self.finish_inner(transaction);
        if result.is_err() {
            self.mark_failed();
        }
        result
    }

    fn finish_inner(
        &mut self,
        transaction: &mut ArtifactBudgetTransaction<'_>,
    ) -> Result<ArtifactPayload, ArtifactPayloadError> {
        if !transaction.owns_scratch_ledger(self.scratch.ledger()) {
            return Err(ArtifactPayloadError::ForeignBudget);
        }

        self.scratch
            .reserve(arc_value_allocation_bytes::<Vec<u8>>()?)?;
        let reservation =
            transaction.preflight_generated(self.bytes.len(), self.bytes.capacity())?;
        self.scratch
            .validate_for_retention(reservation.allocation_bytes())?;
        let backing = Arc::new(mem::take(&mut self.bytes));
        let payload = ArtifactPayload::from_generated_vec(backing)?;
        reservation.finalize(payload.backing().clone());
        self.scratch.release_for_retention();
        Ok(payload)
    }

    fn ensure_active(&self) -> Result<(), ArtifactPayloadError> {
        if self.failed {
            Err(ArtifactPayloadError::PoisonedWriter)
        } else {
            Ok(())
        }
    }

    fn mark_failed(&mut self) {
        self.failed = true;
        if self.poison_transaction_on_error {
            self.scratch.poison_transaction();
        }
    }

    fn write_at_position(&mut self, bytes: &[u8]) -> Result<(), ArtifactPayloadError> {
        let end = self
            .position
            .checked_add(bytes.len())
            .ok_or(ArtifactPayloadError::LengthOverflow { actual: usize::MAX })?;
        self.validate_len(end)?;
        if end > self.bytes.len() {
            let additional = end - self.bytes.len();
            self.scratch
                .grow_vec(&mut self.bytes, additional, "generated chunk bytes")?;
            self.bytes.resize(end, 0);
        }
        self.bytes[self.position..end].copy_from_slice(bytes);
        self.position = end;
        Ok(())
    }

    fn validate_len(&self, length: usize) -> Result<(), ArtifactPayloadError> {
        let requested = payload_len(length)?;
        if requested > self.max_chunk_bytes {
            return Err(ArtifactPayloadError::Budget(
                ArtifactBudgetError::Exceeded {
                    resource: "generated_chunk_bytes",
                    requested,
                    limit: self.max_chunk_bytes,
                },
            ));
        }
        Ok(())
    }
}

impl Write for GeneratedChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.ensure_active().map_err(io::Error::other)?;
        if let Err(error) = self.write_at_position(bytes) {
            self.mark_failed();
            return Err(io::Error::other(error));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ensure_active().map_err(io::Error::other)
    }
}

impl Seek for GeneratedChunkWriter {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.ensure_active().map_err(io::Error::other)?;
        let target = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::End(offset) => self.bytes.len() as i128 + i128::from(offset),
            SeekFrom::Current(offset) => self.position as i128 + i128::from(offset),
        };
        if target < 0 || target > self.bytes.len() as i128 {
            self.mark_failed();
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                ArtifactPayloadError::InvalidSeek {
                    target,
                    buffer_len: self.bytes.len(),
                },
            ));
        }
        let position = match usize::try_from(target) {
            Ok(position) => position,
            Err(_) => {
                self.mark_failed();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    ArtifactPayloadError::InvalidSeek {
                        target,
                        buffer_len: self.bytes.len(),
                    },
                ));
            }
        };
        self.position = position;
        match payload_len(self.position) {
            Ok(position) => Ok(position),
            Err(error) => {
                self.mark_failed();
                Err(io::Error::other(error))
            }
        }
    }
}

fn payload_len(value: usize) -> Result<u64, ArtifactPayloadError> {
    u64::try_from(value).map_err(|_| ArtifactPayloadError::LengthOverflow { actual: value })
}

#[derive(Debug, Error)]
pub enum ArtifactPayloadError {
    #[error(transparent)]
    AllocationSize(#[from] AllocationSizeError),
    #[error(transparent)]
    Budget(#[from] ArtifactBudgetError),
    #[error("payload length {actual} does not fit the artifact length domain")]
    LengthOverflow { actual: usize },
    #[error("source {source_id:?} kind does not match verified image kind {image_kind:?}")]
    SourceKindMismatch {
        source_id: SourceId,
        image_kind: unity_asset_core::SourceKind,
    },
    #[error("generated seek target {target} is outside buffer length {buffer_len}")]
    InvalidSeek { target: i128, buffer_len: usize },
    #[error("generated chunk belongs to a different artifact budget")]
    ForeignBudget,
    #[error("generated chunk writer cannot continue after a failed operation")]
    PoisonedWriter,
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};

    use unity_asset_core::{SourceKind, VerifiedSourceImage, WorkspaceId};

    use super::*;
    use crate::artifact::{ArtifactBudget, ArtifactBudgetUsage, ArtifactLimits};

    fn source_id(kind: SourceKind) -> SourceId {
        SourceId::new(WorkspaceId::from_u128(7).unwrap(), kind, 11).unwrap()
    }

    #[test]
    fn source_payload_consumes_verified_image_and_retains_its_backing() {
        let backing: Arc<[u8]> = Arc::from(b"verified".as_slice());
        let image = VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::clone(&backing));
        let fingerprint = image.fingerprint();

        let payload =
            ArtifactPayload::source_backed(source_id(SourceKind::SerializedFile), image).unwrap();

        assert!(Arc::ptr_eq(
            payload.backing().as_shared_slice_arc().unwrap(),
            &backing
        ));
        assert_eq!(payload.digest(), Some(fingerprint.digest()));
    }

    #[test]
    fn source_payload_rejects_identity_kind_mismatch() {
        let image =
            VerifiedSourceImage::verify(SourceKind::Archive, Arc::from(b"archive".as_slice()));

        assert!(matches!(
            ArtifactPayload::source_backed(source_id(SourceKind::SerializedFile), image),
            Err(ArtifactPayloadError::SourceKindMismatch { .. })
        ));
    }

    #[test]
    fn generated_writer_supports_backpatch_and_promotes_the_same_vec() {
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut transaction = budget.transaction();
        let mut writer = GeneratedChunkWriter::new(&transaction);
        let other = GeneratedChunkWriter::new(&transaction);
        drop(other);

        writer.write_all(b"HEADxxxxbody").unwrap();
        writer.seek(SeekFrom::Start(4)).unwrap();
        writer.write_all(b"0004").unwrap();
        writer.resize_zero(16).unwrap();
        writer.seek(SeekFrom::Start(12)).unwrap();
        writer.write_all(b"tail").unwrap();
        let expected = b"HEAD0004bodytail";
        let payload = writer.finish(&mut transaction).unwrap();

        assert_eq!(payload.bytes(), expected);
        assert_eq!(payload.digest(), None);
        assert!(matches!(
            payload.backing(),
            ArtifactBacking::GeneratedVec(_)
        ));
        let allocation = payload.backing().allocation_bytes().unwrap();
        assert_eq!(transaction.pending_usage().generated_bytes(), allocation);
        assert_eq!(transaction.pending_usage().retained_bytes(), allocation);
    }

    #[test]
    fn failed_generated_promotion_does_not_partially_commit_usage() {
        let mut budget =
            ArtifactBudget::new(ArtifactLimits::default().with_max_generated_bytes(1)).unwrap();
        let mut transaction = budget.transaction();
        let mut writer = GeneratedChunkWriter::new(&transaction);
        writer.write_all(b"too large").unwrap();

        assert!(matches!(
            writer.finish(&mut transaction),
            Err(ArtifactPayloadError::Budget(_))
        ));
        assert_eq!(transaction.pending_usage(), ArtifactBudgetUsage::default());
        assert_eq!(transaction.scratch_ledger().live(), 0);
    }

    #[test]
    fn generated_writer_cannot_finish_in_a_later_transaction() {
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut writer = {
            let transaction = budget.transaction();
            let mut writer = GeneratedChunkWriter::new(&transaction);
            writer.write_all(b"old transaction").unwrap();
            drop(transaction);
            writer
        };
        writer.seek(SeekFrom::End(0)).unwrap();

        let mut next = budget.transaction();
        assert!(matches!(
            writer.finish(&mut next),
            Err(ArtifactPayloadError::ForeignBudget)
        ));
        assert_eq!(next.pending_usage(), ArtifactBudgetUsage::default());
    }

    #[test]
    fn transaction_commit_rejects_a_live_generated_writer() {
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let transaction = budget.transaction();
        let writer = GeneratedChunkWriter::new(&transaction);

        assert!(matches!(
            transaction.commit(),
            Err(ArtifactBudgetError::OutstandingTransactionReservations { outstanding: 1 })
        ));
        drop(writer);

        assert_eq!(budget.committed_usage(), ArtifactBudgetUsage::default());
        assert_eq!(budget.live_scratch_bytes(), 0);
    }

    #[test]
    fn failed_extend_preserves_buffer_and_position() {
        let mut budget =
            ArtifactBudget::new(ArtifactLimits::default().with_max_generated_chunk_bytes(3))
                .unwrap();
        let transaction = budget.transaction();
        let mut writer = GeneratedChunkWriter::new(&transaction);
        writer.write_all(b"ok").unwrap();
        writer.seek(SeekFrom::Start(0)).unwrap();

        assert!(writer.extend_from_slice(b"more").is_err());
        assert_eq!(writer.as_slice(), b"ok");
        assert_eq!(writer.position, 0);
    }
}
