//! Logical source-state contract for incremental search generations.

use std::collections::TryReserveError;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unity_asset_core::{
    AssetLoadBudget, BudgetedJsonError, ChangeSet, Diagnostic, DigestBuildError, DigestV1,
    DigestV1Builder, ObjectAddress, TransactionId, WorkspaceId, WorkspaceRevision,
};
use unity_asset_search_protocol::MAX_PORTABLE_PATH_BYTES;

use crate::analysis::{
    AnalysisTruncation, AssetAnalysis, AssetAnalysisBatch, ContainerEntryFact,
    ReferenceDependencyKey, ReferenceProjectionFact, ReferenceResolutionProjection,
    WorkspaceObjectFact,
};
use crate::generation::{ArtifactTreeEvidence, SearchGenerationManifestV1};
use crate::semantics::AnalysisCacheIdentityV1;
#[cfg(test)]
use crate::semantics::SearchSemantics;

pub(super) const SOURCE_STATE_CONTRACT_VERSION: u16 = 3;
pub(super) const SOURCE_STATE_LOGICAL_IDENTITY_VERSION: u16 = 2;
const MAX_SOURCE_STATE_ASSETS: usize = 1_000_000;
const MAX_SOURCE_STATE_SCAN_HINTS: usize = 1_000_000;
const MAX_TRANSACTION_RECEIPTS: usize = 4_096;
const TRANSACTION_RECEIPT_CONTRACT_VERSION: u16 = 1;
const MAX_SOURCE_STATE_RELATIVE_PATH_BYTES: usize = MAX_PORTABLE_PATH_BYTES;
// Vec starts with at most eight slots for supported non-zero-sized element types, then grows
// geometrically. Internally tagged Serde enums may temporarily buffer one Content sequence/map
// while constructing the final typed Vec. Two independently grown buffers therefore require at
// most sixteen maximum-sized slots per observed array element or object member.
const SOURCE_STATE_CONTAINER_SLOTS_PER_ITEM: u64 = 16;
// serde_json retains one reusable byte scratch Vec for escaped strings. A byte Vec can begin with
// an eight-byte allocation and geometric growth stays below twice the requested length.
const SOURCE_STATE_JSON_SCRATCH_MIN_BYTES: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceStateLimits {
    pub(crate) max_encoded_bytes: u64,
    pub(crate) max_assets: usize,
    pub(crate) max_scan_hints: usize,
    pub(crate) max_relative_path_bytes: usize,
}

impl Default for SourceStateLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 512 * 1024 * 1024,
            max_assets: MAX_SOURCE_STATE_ASSETS,
            max_scan_hints: MAX_SOURCE_STATE_SCAN_HINTS,
            max_relative_path_bytes: MAX_SOURCE_STATE_RELATIVE_PATH_BYTES,
        }
    }
}

/// Durable evidence for one applied workspace transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransactionReceipt {
    contract_version: u16,
    transaction: TransactionId,
    workspace: WorkspaceId,
    from_revision: WorkspaceRevision,
    to_revision: WorkspaceRevision,
    change_set_digest: DigestV1,
}

impl TransactionReceipt {
    fn from_change_set(
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SourceStateError> {
        Ok(Self {
            contract_version: TRANSACTION_RECEIPT_CONTRACT_VERSION,
            transaction: changes.transaction(),
            workspace: changes.workspace(),
            from_revision: changes.from_revision(),
            to_revision: changes.to_revision(),
            change_set_digest: canonical_change_set_digest(changes, budget)?,
        })
    }

    #[must_use]
    pub(crate) const fn transaction(self) -> TransactionId {
        self.transaction
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransactionReceiptMembership {
    Absent,
    Exact,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionReceiptLookup {
    Absent {
        incoming: TransactionReceipt,
    },
    Exact,
    Conflict {
        existing: TransactionReceipt,
        incoming: TransactionReceipt,
    },
}

/// Application-ordered, bounded idempotency ledger.
///
/// The oldest receipt is deterministically evicted when the window is full. Revisions remain the
/// authoritative barrier for transactions older than the retained window. Filesystem
/// reconciliation can advance the enclosing source-state snapshot without consuming a Change Set,
/// so retained receipts may lag the snapshot and adjacent receipts may contain revision gaps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TransactionReceiptWindow {
    receipts: Arc<Vec<TransactionReceipt>>,
}

impl TransactionReceiptWindow {
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self {
            receipts: Arc::new(Vec::new()),
        }
    }

    pub(crate) fn from_change_set(
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SourceStateError> {
        let mut window = Self::empty();
        window.append(changes, budget)?;
        Ok(window)
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn as_slice(&self) -> &[TransactionReceipt] {
        &self.receipts
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn shares_backing_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.receipts, &other.receipts)
    }

    pub(crate) fn ids(&self) -> impl ExactSizeIterator<Item = TransactionId> + '_ {
        self.receipts.iter().map(|receipt| receipt.transaction())
    }

    #[must_use]
    pub(crate) fn matches_canonical_ids(&self, transactions: &[TransactionId]) -> bool {
        self.receipts.len() == transactions.len()
            && self
                .ids()
                .all(|transaction| transactions.binary_search(&transaction).is_ok())
    }

    pub(crate) fn membership(
        &self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<TransactionReceiptMembership, SourceStateError> {
        Ok(match self.lookup(changes, budget)? {
            TransactionReceiptLookup::Absent { .. } => TransactionReceiptMembership::Absent,
            TransactionReceiptLookup::Exact => TransactionReceiptMembership::Exact,
            TransactionReceiptLookup::Conflict { .. } => TransactionReceiptMembership::Conflict,
        })
    }

    pub(crate) fn after_change_set(
        &self,
        indexed_workspace: WorkspaceId,
        indexed_revision: WorkspaceRevision,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SourceStateError> {
        if indexed_workspace != changes.workspace() {
            return Err(SourceStateError::TransactionReceiptWorkspaceMismatch {
                expected: indexed_workspace,
                actual: changes.workspace(),
                transaction: changes.transaction(),
            });
        }
        if indexed_revision != changes.from_revision() {
            return Err(SourceStateError::TransactionReceiptRevisionBarrier {
                indexed: indexed_revision,
                change_from: changes.from_revision(),
                change_to: changes.to_revision(),
            });
        }
        let mut receipts = self.clone();
        receipts.append(changes, budget)?;
        Ok(receipts)
    }

    pub(crate) fn after_reconciled_target(
        &self,
        indexed_workspace: WorkspaceId,
        indexed_revision: WorkspaceRevision,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SourceStateError> {
        if indexed_workspace != changes.workspace() {
            return Err(SourceStateError::TransactionReceiptWorkspaceMismatch {
                expected: indexed_workspace,
                actual: changes.workspace(),
                transaction: changes.transaction(),
            });
        }
        if indexed_revision != changes.to_revision() {
            return Err(SourceStateError::TransactionReceiptRevisionBarrier {
                indexed: indexed_revision,
                change_from: changes.from_revision(),
                change_to: changes.to_revision(),
            });
        }
        let mut receipts = self.clone();
        receipts.append(changes, budget)?;
        Ok(receipts)
    }

    fn lookup(
        &self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<TransactionReceiptLookup, SourceStateError> {
        let incoming = TransactionReceipt::from_change_set(changes, budget)?;
        Ok(
            match self
                .receipts
                .iter()
                .find(|receipt| receipt.transaction == incoming.transaction)
                .copied()
            {
                Some(existing) if existing == incoming => TransactionReceiptLookup::Exact,
                Some(existing) => TransactionReceiptLookup::Conflict { existing, incoming },
                None => TransactionReceiptLookup::Absent { incoming },
            },
        )
    }

    pub(crate) fn append(
        &mut self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SourceStateError> {
        validate_source_state_count(
            "transaction receipts",
            self.receipts.len(),
            MAX_TRANSACTION_RECEIPTS,
        )?;
        let incoming = match self.lookup(changes, budget)? {
            TransactionReceiptLookup::Exact => return Ok(()),
            TransactionReceiptLookup::Conflict { existing, incoming } => {
                return Err(SourceStateError::TransactionConflict {
                    existing: Box::new(existing),
                    incoming: Box::new(incoming),
                });
            }
            TransactionReceiptLookup::Absent { incoming } => incoming,
        };
        let receipt_bytes = std::mem::size_of::<TransactionReceipt>() as u64;
        budget
            .check_entries(1)
            .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
        budget
            .check_bytes(receipt_bytes)
            .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
        budget
            .consume_entries(1)
            .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
        budget
            .consume_bytes(receipt_bytes)
            .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
        if let Some(receipts) = Arc::get_mut(&mut self.receipts) {
            if receipts.len() == MAX_TRANSACTION_RECEIPTS {
                receipts.remove(0);
            } else {
                receipts.try_reserve_exact(1).map_err(|source| {
                    SourceStateError::AllocationFailed {
                        requested: receipt_bytes as usize,
                        source,
                    }
                })?;
            }
            receipts.push(incoming);
            return Ok(());
        }

        let first_retained = usize::from(self.receipts.len() == MAX_TRANSACTION_RECEIPTS);
        let retained = &self.receipts[first_retained..];
        let entries =
            u64::try_from(retained.len()).map_err(|_| SourceStateError::SizeOverflow {
                resource: "transaction receipt entries",
            })?;
        let bytes = entries
            .checked_mul(std::mem::size_of::<TransactionReceipt>() as u64)
            .ok_or(SourceStateError::SizeOverflow {
                resource: "transaction receipt bytes",
            })?;
        budget
            .check_entries(entries)
            .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
        budget
            .check_bytes(bytes)
            .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
        budget
            .consume_entries(entries)
            .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
        budget
            .consume_bytes(bytes)
            .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
        let mut receipts = Vec::new();
        receipts
            .try_reserve_exact(retained.len().saturating_add(1))
            .map_err(|source| SourceStateError::AllocationFailed {
                requested: retained
                    .len()
                    .saturating_add(1)
                    .saturating_mul(std::mem::size_of::<TransactionReceipt>()),
                source,
            })?;
        receipts.extend_from_slice(retained);
        receipts.push(incoming);
        self.receipts = Arc::new(receipts);
        Ok(())
    }

    pub(crate) fn validate_for_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<(), SourceStateError> {
        let maximum = MAX_TRANSACTION_RECEIPTS;
        validate_source_state_count("transaction receipts", self.receipts.len(), maximum)?;
        for (index, receipt) in self.receipts.iter().enumerate() {
            if receipt.contract_version != TRANSACTION_RECEIPT_CONTRACT_VERSION {
                return Err(SourceStateError::UnsupportedTransactionReceiptVersion {
                    actual: receipt.contract_version,
                    expected: TRANSACTION_RECEIPT_CONTRACT_VERSION,
                });
            }
            if receipt.workspace != workspace {
                return Err(SourceStateError::TransactionReceiptWorkspaceMismatch {
                    expected: workspace,
                    actual: receipt.workspace,
                    transaction: receipt.transaction,
                });
            }
            if receipt.from_revision == receipt.to_revision {
                return Err(SourceStateError::TransactionReceiptDidNotAdvance {
                    transaction: receipt.transaction,
                    revision: receipt.from_revision,
                });
            }
            if self.receipts[..index]
                .iter()
                .any(|previous| previous.transaction == receipt.transaction)
            {
                return Err(SourceStateError::DuplicateTransactionReceipt {
                    transaction: receipt.transaction,
                });
            }
        }
        // Workspace revisions are content digests, not logical clocks. Their relative chronology
        // cannot be validated from their values. Normal publication binds the window to one
        // generation's source state, while transaction_receipts_after enforces that every newly
        // consumed Change Set starts at that source state's current snapshot revision.
        Ok(())
    }
}

impl Serialize for TransactionReceiptWindow {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.receipts.as_slice().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TransactionReceiptWindow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<TransactionReceipt>::deserialize(deserializer).map(|receipts| Self {
            receipts: Arc::new(receipts),
        })
    }
}

/// Filesystem metadata used only as a fast unchanged-source hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceScanHint {
    pub(crate) relative_path: String,
    pub(crate) source_length: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_modified_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata_modified_unix_ms: Option<u64>,
}

impl SourceScanHint {
    pub(crate) fn new(
        relative_path: String,
        source_length: u64,
        source_modified_unix_ms: Option<u64>,
        metadata_length: Option<u64>,
        metadata_modified_unix_ms: Option<u64>,
    ) -> Result<Self, SourceStateError> {
        validate_source_state_relative_path(&relative_path, MAX_SOURCE_STATE_RELATIVE_PATH_BYTES)?;
        Ok(Self {
            relative_path,
            source_length,
            source_modified_unix_ms,
            metadata_length,
            metadata_modified_unix_ms,
        })
    }
}

/// Canonical, generation-bound cache of the analysis pass.
///
/// Scan hints are excluded from the logical digest because timestamps are not content identity.
/// They are covered by physical artifact evidence and must never be used as correctness evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SourceStateSnapshot {
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    analysis_cache_identity: AnalysisCacheIdentityV1,
    scan_hints: Vec<SourceScanHint>,
    assets: Vec<AssetAnalysis>,
    logical_digest: DigestV1,
}

impl SourceStateSnapshot {
    pub(crate) fn from_batch(
        batch: AssetAnalysisBatch,
        scan_hints: Vec<SourceScanHint>,
        analysis_cache_identity: AnalysisCacheIdentityV1,
    ) -> Result<Self, SourceStateError> {
        Self::new_with_analysis_cache_identity(
            batch.workspace,
            batch.revision,
            analysis_cache_identity,
            scan_hints,
            batch.assets,
        )
    }

    #[cfg(test)]
    pub(crate) fn new(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        scan_hints: Vec<SourceScanHint>,
        assets: Vec<AssetAnalysis>,
    ) -> Result<Self, SourceStateError> {
        let analysis_cache_identity = SearchSemantics::current()
            .analysis_cache_identity(DigestV1::hash_bytes(b"options"))
            .map_err(SourceStateError::Digest)?;
        Self::new_with_analysis_cache_identity(
            workspace,
            revision,
            analysis_cache_identity,
            scan_hints,
            assets,
        )
    }

    fn new_with_analysis_cache_identity(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        analysis_cache_identity: AnalysisCacheIdentityV1,
        mut scan_hints: Vec<SourceScanHint>,
        mut assets: Vec<AssetAnalysis>,
    ) -> Result<Self, SourceStateError> {
        validate_source_state_count("scan hints", scan_hints.len(), MAX_SOURCE_STATE_SCAN_HINTS)?;
        validate_source_state_count("assets", assets.len(), MAX_SOURCE_STATE_ASSETS)?;

        for analysis in &mut assets {
            normalize_asset_analysis(analysis);
        }
        scan_hints.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
        reject_duplicate_source_state_paths(
            "scan hints",
            scan_hints.iter().map(|hint| hint.relative_path.as_str()),
        )?;
        assets.sort_unstable_by(|left, right| {
            left.source.relative_path.cmp(&right.source.relative_path)
        });
        reject_duplicate_source_state_paths(
            "assets",
            assets
                .iter()
                .map(|analysis| analysis.source.relative_path.as_str()),
        )?;
        for hint in &scan_hints {
            validate_source_state_relative_path(
                &hint.relative_path,
                MAX_SOURCE_STATE_RELATIVE_PATH_BYTES,
            )?;
        }
        for analysis in &assets {
            validate_source_state_relative_path(
                &analysis.source.relative_path,
                MAX_SOURCE_STATE_RELATIVE_PATH_BYTES,
            )?;
        }

        let logical_digest =
            source_state_logical_digest(workspace, revision, analysis_cache_identity, &assets)?;
        Ok(Self {
            contract_version: SOURCE_STATE_CONTRACT_VERSION,
            workspace,
            revision,
            analysis_cache_identity,
            scan_hints,
            assets,
            logical_digest,
        })
    }

    #[must_use]
    pub(crate) const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub(crate) const fn analysis_cache_identity(&self) -> AnalysisCacheIdentityV1 {
        self.analysis_cache_identity
    }

    #[must_use]
    pub(crate) fn scan_hints(&self) -> &[SourceScanHint] {
        &self.scan_hints
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn scan_hint(&self, relative_path: &str) -> Option<&SourceScanHint> {
        self.scan_hints
            .binary_search_by(|hint| hint.relative_path.as_str().cmp(relative_path))
            .ok()
            .and_then(|index| self.scan_hints.get(index))
    }

    #[must_use]
    pub(crate) fn assets(&self) -> &[AssetAnalysis] {
        &self.assets
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn analysis(&self, relative_path: &str) -> Option<&AssetAnalysis> {
        self.assets
            .binary_search_by(|analysis| analysis.source.relative_path.as_str().cmp(relative_path))
            .ok()
            .and_then(|index| self.assets.get(index))
    }

    #[must_use]
    pub(crate) const fn logical_digest(&self) -> DigestV1 {
        self.logical_digest
    }

    #[cfg(test)]
    pub(crate) fn rebind_analysis_cache_identity(
        self,
        analysis_cache_identity: AnalysisCacheIdentityV1,
    ) -> Result<Self, SourceStateError> {
        Self::new_with_analysis_cache_identity(
            self.workspace,
            self.revision,
            analysis_cache_identity,
            self.scan_hints,
            self.assets,
        )
    }

    pub(super) fn validate_limits(
        &self,
        limits: SourceStateLimits,
    ) -> Result<(), SourceStateError> {
        validate_source_state_count("scan hints", self.scan_hints.len(), limits.max_scan_hints)?;
        validate_source_state_count("assets", self.assets.len(), limits.max_assets)?;
        for hint in &self.scan_hints {
            validate_source_state_relative_path(
                &hint.relative_path,
                limits.max_relative_path_bytes,
            )?;
        }
        for analysis in &self.assets {
            validate_source_state_relative_path(
                &analysis.source.relative_path,
                limits.max_relative_path_bytes,
            )?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceStateSnapshotWire {
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    analysis_cache_identity: AnalysisCacheIdentityV1,
    scan_hints: Vec<SourceScanHint>,
    assets: Vec<AssetAnalysis>,
    logical_digest: DigestV1,
}

impl<'de> Deserialize<'de> for SourceStateSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SourceStateSnapshotWire::deserialize(deserializer)?;
        if wire.contract_version != SOURCE_STATE_CONTRACT_VERSION {
            return Err(serde::de::Error::custom(
                SourceStateError::UnsupportedVersion {
                    actual: wire.contract_version,
                    expected: SOURCE_STATE_CONTRACT_VERSION,
                },
            ));
        }
        ensure_source_state_canonical_order(&wire.scan_hints, &wire.assets)
            .map_err(serde::de::Error::custom)?;
        ensure_source_state_assets_canonical(&wire.assets).map_err(serde::de::Error::custom)?;
        let snapshot = Self::new_with_analysis_cache_identity(
            wire.workspace,
            wire.revision,
            wire.analysis_cache_identity,
            wire.scan_hints,
            wire.assets,
        )
        .map_err(serde::de::Error::custom)?;
        if snapshot.logical_digest != wire.logical_digest {
            return Err(serde::de::Error::custom(
                SourceStateError::LogicalDigestMismatch {
                    expected: wire.logical_digest,
                    actual: snapshot.logical_digest,
                },
            ));
        }
        Ok(snapshot)
    }
}

#[derive(Serialize)]
struct SourceStateLogicalRef<'state> {
    identity_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    analysis_cache_identity: AnalysisCacheIdentityV1,
    assets: &'state [AssetAnalysis],
}

fn source_state_logical_digest(
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    analysis_cache_identity: AnalysisCacheIdentityV1,
    assets: &[AssetAnalysis],
) -> Result<DigestV1, SourceStateError> {
    let logical = SourceStateLogicalRef {
        identity_version: SOURCE_STATE_LOGICAL_IDENTITY_VERSION,
        workspace,
        revision,
        analysis_cache_identity,
        assets,
    };
    canonical_digest(&logical)
}

fn canonical_digest(value: &impl Serialize) -> Result<DigestV1, SourceStateError> {
    let encoded_length = canonical_json_length(value)?;
    digest_canonical_json(value, encoded_length)
}

fn canonical_change_set_digest(
    changes: &ChangeSet,
    budget: &mut AssetLoadBudget,
) -> Result<DigestV1, SourceStateError> {
    let encoded_length = canonical_json_length(changes)?;
    let entries = changes
        .changed_sources()
        .len()
        .checked_add(changes.changed_objects().len())
        .and_then(|count| count.checked_add(changes.identity_remaps().len()))
        .and_then(|count| u64::try_from(count).ok())
        .ok_or(SourceStateError::SizeOverflow {
            resource: "change set entries",
        })?;
    budget
        .check_entries(entries)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .check_bytes(encoded_length)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .consume_entries(entries)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .consume_bytes(encoded_length)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    digest_canonical_json(changes, encoded_length)
}

fn digest_canonical_json(
    value: &impl Serialize,
    encoded_length: u64,
) -> Result<DigestV1, SourceStateError> {
    let mut digest = DigestV1Builder::new(encoded_length);
    serde_json::to_writer(DigestWriter(&mut digest), value).map_err(SourceStateError::Json)?;
    digest.finalize().map_err(SourceStateError::Digest)
}

fn canonical_json_length(value: &impl Serialize) -> Result<u64, SourceStateError> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value).map_err(SourceStateError::Json)?;
    Ok(counter.bytes)
}

#[derive(Default)]
pub(super) struct ByteCounter {
    pub(super) bytes: u64,
}

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let amount = u64::try_from(buffer.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "JSON length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(amount)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "JSON length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct SizeLimitedWriter<W> {
    inner: W,
    written: u64,
    maximum: u64,
    rejected_bytes: Option<u64>,
}

impl<W> SizeLimitedWriter<W> {
    pub(super) const fn new(inner: W, maximum: u64) -> Self {
        Self {
            inner,
            written: 0,
            maximum,
            rejected_bytes: None,
        }
    }

    pub(super) const fn rejected_bytes(&self) -> Option<u64> {
        self.rejected_bytes
    }

    pub(super) const fn inner(&self) -> &W {
        &self.inner
    }
}

impl<W: Write> Write for SizeLimitedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let amount = u64::try_from(buffer.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "JSON length overflow"))?;
        let requested = self
            .written
            .checked_add(amount)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "JSON length overflow"))?;
        if requested > self.maximum {
            self.rejected_bytes = Some(requested);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "source state exceeds its encoded byte limit",
            ));
        }
        let written = self.inner.write(buffer)?;
        self.written = self
            .written
            .checked_add(
                u64::try_from(written).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "JSON length overflow")
                })?,
            )
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "JSON length overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct DigestWriter<'digest>(&'digest mut DigestV1Builder);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .update(buffer)
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn ensure_source_state_canonical_order(
    scan_hints: &[SourceScanHint],
    assets: &[AssetAnalysis],
) -> Result<(), SourceStateError> {
    ensure_strictly_sorted_paths(
        "scan hints",
        scan_hints.iter().map(|hint| hint.relative_path.as_str()),
    )?;
    ensure_strictly_sorted_paths(
        "assets",
        assets
            .iter()
            .map(|analysis| analysis.source.relative_path.as_str()),
    )
}

fn normalize_asset_analysis(analysis: &mut AssetAnalysis) {
    sort_deduplicate(&mut analysis.search.hierarchy_paths);
    sort_deduplicate(&mut analysis.search.script_symbols);
    sort_deduplicate(&mut analysis.search.referenced_script_guids);
    analysis.graph_inputs.objects.sort_unstable();
    analysis
        .graph_inputs
        .objects
        .dedup_by(|left, right| left.address == right.address);
    for reference in &mut analysis.references {
        sort_deduplicate(&mut reference.diagnostics);
        sort_deduplicate(&mut reference.dependency_keys);
    }
    sort_deduplicate(&mut analysis.references);
    sort_deduplicate(&mut analysis.container_entries);
    sort_deduplicate(&mut analysis.diagnostics);
    sort_deduplicate(&mut analysis.truncations);
    if !analysis.truncations.is_empty() {
        analysis.complete = false;
    }
}

fn ensure_source_state_assets_canonical(assets: &[AssetAnalysis]) -> Result<(), SourceStateError> {
    for analysis in assets {
        let canonical = is_strictly_sorted(&analysis.search.hierarchy_paths)
            && is_strictly_sorted(&analysis.search.script_symbols)
            && is_strictly_sorted(&analysis.search.referenced_script_guids)
            && analysis
                .graph_inputs
                .objects
                .windows(2)
                .all(|pair| pair[0].address < pair[1].address)
            && is_strictly_sorted(&analysis.references)
            && is_strictly_sorted(&analysis.container_entries)
            && is_strictly_sorted(&analysis.diagnostics)
            && is_strictly_sorted(&analysis.truncations)
            && (analysis.truncations.is_empty() || !analysis.complete)
            && analysis.references.iter().all(|reference| {
                is_strictly_sorted(&reference.diagnostics)
                    && is_strictly_sorted(&reference.dependency_keys)
            });
        if !canonical {
            return Err(SourceStateError::NonCanonicalAnalysis {
                relative_path: analysis.source.relative_path.clone(),
            });
        }
    }
    Ok(())
}

fn sort_deduplicate<T: Ord>(values: &mut Vec<T>) {
    values.sort_unstable();
    values.dedup();
}

fn is_strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn ensure_strictly_sorted_paths<'path>(
    collection: &'static str,
    paths: impl IntoIterator<Item = &'path str>,
) -> Result<(), SourceStateError> {
    let mut previous = None;
    for path in paths {
        if matches!(previous, Some(previous) if previous >= path) {
            return Err(SourceStateError::NonCanonicalOrder { collection });
        }
        previous = Some(path);
    }
    Ok(())
}

fn reject_duplicate_source_state_paths<'path>(
    collection: &'static str,
    paths: impl IntoIterator<Item = &'path str>,
) -> Result<(), SourceStateError> {
    let mut previous = None;
    for path in paths {
        if previous == Some(path) {
            return Err(SourceStateError::DuplicateRelativePath {
                collection,
                relative_path: path.to_owned(),
            });
        }
        previous = Some(path);
    }
    Ok(())
}

fn validate_source_state_count(
    collection: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), SourceStateError> {
    if actual > maximum {
        return Err(SourceStateError::CollectionTooLarge {
            collection,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn validate_source_state_relative_path(
    relative_path: &str,
    maximum_bytes: usize,
) -> Result<(), SourceStateError> {
    if relative_path.is_empty()
        || relative_path.len() > maximum_bytes
        || relative_path.starts_with('/')
        || relative_path
            .chars()
            .any(|character| matches!(character, '\\' | '\0' | ':'))
        || relative_path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(SourceStateError::InvalidRelativePath {
            relative_path: relative_path.to_owned(),
            maximum_bytes,
        });
    }
    Ok(())
}

pub(super) fn validate_source_state_manifest(
    snapshot: &SourceStateSnapshot,
    manifest: &SearchGenerationManifestV1,
) -> Result<(), SourceStateError> {
    if snapshot.workspace != manifest.workspace() || snapshot.revision != manifest.revision() {
        return Err(SourceStateError::GenerationContextMismatch {
            expected_workspace: manifest.workspace(),
            actual_workspace: snapshot.workspace,
            expected_revision: manifest.revision(),
            actual_revision: snapshot.revision,
        });
    }
    if snapshot.logical_digest != manifest.source_state_digest() {
        return Err(SourceStateError::ManifestDigestMismatch {
            expected: manifest.source_state_digest(),
            actual: snapshot.logical_digest,
        });
    }
    let expected_analysis_cache_identity = manifest
        .semantics()
        .analysis_cache_identity(manifest.options_digest())
        .map_err(SourceStateError::Digest)?;
    if snapshot.analysis_cache_identity != expected_analysis_cache_identity {
        return Err(SourceStateError::ManifestAnalysisCacheIdentityMismatch {
            expected: expected_analysis_cache_identity,
            actual: snapshot.analysis_cache_identity,
        });
    }
    Ok(())
}

pub(super) fn source_state_entry_count(
    snapshot: &SourceStateSnapshot,
) -> Result<u64, SourceStateError> {
    source_state_entry_count_parts(snapshot.scan_hints.len(), &snapshot.assets, 0)
}

fn source_state_entry_count_parts(
    scan_hints: usize,
    assets: &[AssetAnalysis],
    additional_entries: usize,
) -> Result<u64, SourceStateError> {
    let mut entries = scan_hints
        .checked_add(assets.len())
        .and_then(|count| count.checked_add(additional_entries))
        .ok_or(SourceStateError::SizeOverflow {
            resource: "source state entries",
        })?;
    for analysis in assets {
        for count in [
            analysis.search.hierarchy_paths.len(),
            analysis.search.script_symbols.len(),
            analysis.search.referenced_script_guids.len(),
            analysis.graph_inputs.objects.len(),
            analysis.references.len(),
            analysis.container_entries.len(),
            analysis.diagnostics.len(),
            analysis.truncations.len(),
        ] {
            entries = entries
                .checked_add(count)
                .ok_or(SourceStateError::SizeOverflow {
                    resource: "source state entries",
                })?;
        }
        for reference in &analysis.references {
            entries = entries
                .checked_add(reference.diagnostics.len())
                .and_then(|count| count.checked_add(reference.dependency_keys.len()))
                .ok_or(SourceStateError::SizeOverflow {
                    resource: "source state entries",
                })?;
            if let ReferenceResolutionProjection::Ambiguous { candidates } = &reference.resolution {
                entries = entries.checked_add(candidates.len()).ok_or(
                    SourceStateError::SizeOverflow {
                        resource: "source state entries",
                    },
                )?;
            }
        }
    }
    u64::try_from(entries).map_err(|_| SourceStateError::SizeOverflow {
        resource: "source state entries",
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JsonStructure {
    pub(super) array_entries: u64,
    pub(super) object_members: u64,
    // JSON escape syntax never decodes to more UTF-8 bytes than its raw string body.
    pub(super) string_backing_bytes: u64,
    pub(super) max_escaped_string_body_bytes: u64,
    pub(super) max_depth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonContainer {
    Array { expects_value: bool },
    Object,
}

fn mark_json_array_value(
    containers: &mut [Option<JsonContainer>],
    depth: usize,
    array_entries: &mut u64,
) -> Result<(), SourceStateError> {
    let Some(JsonContainer::Array { expects_value }) = depth
        .checked_sub(1)
        .and_then(|slot| containers[slot].as_mut())
    else {
        return Ok(());
    };
    if *expects_value {
        *array_entries = array_entries
            .checked_add(1)
            .ok_or(SourceStateError::SizeOverflow {
                resource: "source state structural entries",
            })?;
        *expects_value = false;
    }
    Ok(())
}

pub(super) fn scan_json_structure(encoded: &[u8]) -> Result<JsonStructure, SourceStateError> {
    const MAX_TRACKED_DEPTH: usize = 64;

    let mut in_string = false;
    let mut escaped = false;
    let mut in_primitive = false;
    let mut containers = [None; MAX_TRACKED_DEPTH];
    let mut depth = 0_usize;
    let mut max_depth = 0_u32;
    let mut array_entries = 0_u64;
    let mut object_members = 0_u64;
    let mut string_backing_bytes = 0_u64;
    let mut current_string_body_bytes = 0_u64;
    let mut current_string_has_escape = false;
    let mut max_escaped_string_body_bytes = 0_u64;
    let mut index = 0_usize;
    while let Some(byte) = encoded.get(index).copied() {
        if in_string {
            if !escaped && byte == b'"' {
                if current_string_has_escape {
                    max_escaped_string_body_bytes =
                        max_escaped_string_body_bytes.max(current_string_body_bytes);
                }
                in_string = false;
            } else {
                string_backing_bytes =
                    string_backing_bytes
                        .checked_add(1)
                        .ok_or(SourceStateError::SizeOverflow {
                            resource: "source state string backing",
                        })?;
                current_string_body_bytes = current_string_body_bytes.checked_add(1).ok_or(
                    SourceStateError::SizeOverflow {
                        resource: "source state current string backing",
                    },
                )?;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    current_string_has_escape = true;
                    escaped = true;
                }
            }
            index += 1;
            continue;
        }
        if in_primitive {
            if matches!(
                byte,
                b',' | b']' | b'}' | b'[' | b'{' | b':' | b'"' | b' ' | b'\t' | b'\r' | b'\n'
            ) {
                in_primitive = false;
                if byte.is_ascii_whitespace() {
                    index += 1;
                    continue;
                }
            } else {
                index += 1;
                continue;
            }
        }

        match byte {
            b'"' => {
                mark_json_array_value(&mut containers, depth, &mut array_entries)?;
                current_string_body_bytes = 0;
                current_string_has_escape = false;
                in_string = true;
            }
            b'[' | b'{' => {
                mark_json_array_value(&mut containers, depth, &mut array_entries)?;
                if depth == MAX_TRACKED_DEPTH {
                    return Err(SourceStateError::JsonStructureDepthExceeded {
                        actual: MAX_TRACKED_DEPTH + 1,
                        maximum: MAX_TRACKED_DEPTH,
                    });
                }
                containers[depth] = Some(if byte == b'[' {
                    JsonContainer::Array {
                        expects_value: true,
                    }
                } else {
                    JsonContainer::Object
                });
                depth += 1;
                max_depth = max_depth.max(u32::try_from(depth).map_err(|_| {
                    SourceStateError::SizeOverflow {
                        resource: "source state JSON depth",
                    }
                })?);
            }
            b']' => {
                if depth != 0 {
                    depth -= 1;
                    containers[depth] = None;
                }
            }
            b'}' => {
                if depth != 0 {
                    depth -= 1;
                    containers[depth] = None;
                }
            }
            b',' => {
                if let Some(JsonContainer::Array { expects_value }) = depth
                    .checked_sub(1)
                    .and_then(|slot| containers[slot].as_mut())
                {
                    *expects_value = true;
                }
            }
            b':' => {
                object_members =
                    object_members
                        .checked_add(1)
                        .ok_or(SourceStateError::SizeOverflow {
                            resource: "source state structural members",
                        })?;
            }
            b' ' | b'\t' | b'\r' | b'\n' => {}
            _ => {
                mark_json_array_value(&mut containers, depth, &mut array_entries)?;
                in_primitive = true;
            }
        }
        index += 1;
    }
    if in_string && current_string_has_escape {
        max_escaped_string_body_bytes =
            max_escaped_string_body_bytes.max(current_string_body_bytes);
    }
    Ok(JsonStructure {
        array_entries,
        object_members,
        string_backing_bytes,
        max_escaped_string_body_bytes,
        max_depth,
    })
}

pub(super) fn source_state_owned_allocation_bound(
    structure: JsonStructure,
) -> Result<u64, SourceStateError> {
    // Every persisted Vec element has one of these layouts. Charging the maximum for each exact
    // structural item deliberately overestimates mixed arrays while avoiding an encoded-length
    // heuristic. Raw string bodies independently bound every owned String backing allocation.
    let maximum_slot = [
        std::mem::size_of::<TransactionReceipt>(),
        std::mem::size_of::<SourceScanHint>(),
        std::mem::size_of::<AssetAnalysis>(),
        std::mem::size_of::<WorkspaceObjectFact>(),
        std::mem::size_of::<ReferenceProjectionFact>(),
        std::mem::size_of::<ContainerEntryFact>(),
        std::mem::size_of::<Diagnostic>(),
        std::mem::size_of::<AnalysisTruncation>(),
        std::mem::size_of::<ReferenceDependencyKey>(),
        std::mem::size_of::<ObjectAddress>(),
        std::mem::size_of::<String>(),
    ]
    .into_iter()
    .max()
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or(SourceStateError::SizeOverflow {
        resource: "source state maximum array slot",
    })?;
    let container_items = structure
        .array_entries
        .checked_add(structure.object_members)
        .ok_or(SourceStateError::SizeOverflow {
            resource: "source state container items",
        })?;
    let container_backing = container_items
        .checked_mul(SOURCE_STATE_CONTAINER_SLOTS_PER_ITEM)
        .and_then(|slots| slots.checked_mul(maximum_slot))
        .ok_or(SourceStateError::SizeOverflow {
            resource: "source state container backing",
        })?;
    let json_scratch = if structure.max_escaped_string_body_bytes == 0 {
        0
    } else {
        structure
            .max_escaped_string_body_bytes
            .checked_mul(2)
            .ok_or(SourceStateError::SizeOverflow {
                resource: "source state escaped string scratch",
            })?
            .max(SOURCE_STATE_JSON_SCRATCH_MIN_BYTES)
    };
    structure
        .string_backing_bytes
        .checked_add(json_scratch)
        .ok_or(SourceStateError::SizeOverflow {
            resource: "source state string allocation",
        })?
        .checked_add(container_backing)
        .ok_or(SourceStateError::SizeOverflow {
            resource: "source state owned allocation",
        })
}

#[derive(Debug)]
pub(crate) enum SourceStateError {
    Budget(BudgetedJsonError),
    Json(serde_json::Error),
    Digest(DigestBuildError),
    UnsupportedVersion {
        actual: u16,
        expected: u16,
    },
    CollectionTooLarge {
        collection: &'static str,
        actual: usize,
        maximum: usize,
    },
    UnsupportedTransactionReceiptVersion {
        actual: u16,
        expected: u16,
    },
    DuplicateTransactionReceipt {
        transaction: TransactionId,
    },
    TransactionConflict {
        existing: Box<TransactionReceipt>,
        incoming: Box<TransactionReceipt>,
    },
    TransactionReceiptWorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
        transaction: TransactionId,
    },
    TransactionReceiptDidNotAdvance {
        transaction: TransactionId,
        revision: WorkspaceRevision,
    },
    TransactionReceiptRevisionBarrier {
        indexed: WorkspaceRevision,
        change_from: WorkspaceRevision,
        change_to: WorkspaceRevision,
    },
    NonCanonicalOrder {
        collection: &'static str,
    },
    NonCanonicalAnalysis {
        relative_path: String,
    },
    DuplicateRelativePath {
        collection: &'static str,
        relative_path: String,
    },
    InvalidRelativePath {
        relative_path: String,
        maximum_bytes: usize,
    },
    LogicalDigestMismatch {
        expected: DigestV1,
        actual: DigestV1,
    },
    ManifestDigestMismatch {
        expected: DigestV1,
        actual: DigestV1,
    },
    ManifestAnalysisCacheIdentityMismatch {
        expected: AnalysisCacheIdentityV1,
        actual: AnalysisCacheIdentityV1,
    },
    PhysicalEvidenceMismatch {
        expected: ArtifactTreeEvidence,
        actual: ArtifactTreeEvidence,
    },
    GenerationContextMismatch {
        expected_workspace: WorkspaceId,
        actual_workspace: WorkspaceId,
        expected_revision: WorkspaceRevision,
        actual_revision: WorkspaceRevision,
    },
    EncodedTooLarge {
        actual: u64,
        maximum: u64,
    },
    EncodedLengthChanged {
        expected: u64,
        actual: u64,
    },
    AllocationFailed {
        requested: usize,
        source: TryReserveError,
    },
    StructuralEntryUnderestimate {
        structural: u64,
        semantic: u64,
    },
    JsonStructureDepthExceeded {
        actual: usize,
        maximum: usize,
    },
    SizeOverflow {
        resource: &'static str,
    },
}

impl fmt::Display for SourceStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::Json(error) => write!(formatter, "invalid source state JSON: {error}"),
            Self::Digest(error) => write!(formatter, "failed to digest source state: {error}"),
            Self::UnsupportedVersion { actual, expected } => write!(
                formatter,
                "source state version {actual} is unsupported; expected {expected}"
            ),
            Self::CollectionTooLarge {
                collection,
                actual,
                maximum,
            } => write!(
                formatter,
                "source state {collection} contains {actual} items; maximum is {maximum}"
            ),
            Self::UnsupportedTransactionReceiptVersion { actual, expected } => write!(
                formatter,
                "transaction receipt version {actual} is unsupported; expected {expected}"
            ),
            Self::DuplicateTransactionReceipt { transaction } => {
                write!(formatter, "transaction receipt {transaction} is duplicated")
            }
            Self::TransactionConflict { existing, incoming } => write!(
                formatter,
                "transaction {} conflicts with its durable receipt: stored change-set digest {}, incoming digest {}",
                existing.transaction, existing.change_set_digest, incoming.change_set_digest
            ),
            Self::TransactionReceiptWorkspaceMismatch {
                expected,
                actual,
                transaction,
            } => write!(
                formatter,
                "transaction {transaction} belongs to workspace {actual}, not {expected}"
            ),
            Self::TransactionReceiptDidNotAdvance {
                transaction,
                revision,
            } => write!(
                formatter,
                "transaction {transaction} does not advance revision {revision}"
            ),
            Self::TransactionReceiptRevisionBarrier {
                indexed,
                change_from,
                change_to,
            } => write!(
                formatter,
                "indexed revision {indexed} cannot apply transaction from {change_from} to {change_to}"
            ),
            Self::NonCanonicalOrder { collection } => {
                write!(
                    formatter,
                    "source state {collection} are not sorted and unique"
                )
            }
            Self::NonCanonicalAnalysis { relative_path } => write!(
                formatter,
                "source state analysis for {relative_path} is not canonical"
            ),
            Self::DuplicateRelativePath {
                collection,
                relative_path,
            } => write!(
                formatter,
                "source state {collection} contain duplicate path {relative_path}"
            ),
            Self::InvalidRelativePath {
                relative_path,
                maximum_bytes,
            } => write!(
                formatter,
                "source state path {relative_path:?} is not a portable relative path within {maximum_bytes} bytes"
            ),
            Self::LogicalDigestMismatch { expected, actual } => write!(
                formatter,
                "source state logical digest mismatch: expected {expected}, got {actual}"
            ),
            Self::ManifestDigestMismatch { expected, actual } => write!(
                formatter,
                "source state does not match generation manifest: expected {expected}, got {actual}"
            ),
            Self::ManifestAnalysisCacheIdentityMismatch { expected, actual } => write!(
                formatter,
                "source state analysis cache identity does not match generation manifest: expected {expected:?}, got {actual:?}"
            ),
            Self::PhysicalEvidenceMismatch { expected, actual } => write!(
                formatter,
                "source state physical evidence mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::GenerationContextMismatch {
                expected_workspace,
                actual_workspace,
                expected_revision,
                actual_revision,
            } => write!(
                formatter,
                "source state context mismatch: expected {expected_workspace}/{expected_revision}, got {actual_workspace}/{actual_revision}"
            ),
            Self::EncodedTooLarge { actual, maximum } => write!(
                formatter,
                "encoded source state contains {actual} bytes; maximum is {maximum}"
            ),
            Self::EncodedLengthChanged { expected, actual } => write!(
                formatter,
                "encoded source state length changed while reading: expected {expected} bytes, got {actual}"
            ),
            Self::AllocationFailed { requested, source } => write!(
                formatter,
                "failed to reserve {requested} bytes for source state: {source}"
            ),
            Self::StructuralEntryUnderestimate {
                structural,
                semantic,
            } => write!(
                formatter,
                "source state structural entry count {structural} is below semantic count {semantic}"
            ),
            Self::JsonStructureDepthExceeded { actual, maximum } => write!(
                formatter,
                "source state JSON depth {actual} exceeds structural scanner maximum {maximum}"
            ),
            Self::SizeOverflow { resource } => write!(formatter, "{resource} size overflow"),
        }
    }
}

impl Error for SourceStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Digest(error) => Some(error),
            Self::AllocationFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod source_state_tests {
    use tempfile::TempDir;
    use unity_asset_core::{
        AssetLoadLimits, DigestV1, ObjectAddress, SourceId, SourceKind, SourceLocator,
    };
    use unity_asset_search_core::SearchKind;

    use super::super::*;
    use super::*;
    use crate::analysis::{
        AnalysisTruncation, AnalysisTruncationKind, AnalyzedSource, AssetAnalysis, SearchFacts,
        WorkspaceObjectFact,
    };
    use crate::generation::{
        ArtifactTreeEvidence, GenerationArtifactEvidence, GenerationProjectionDigests,
        SearchGenerationIdentityV1, SearchGenerationManifestV1,
    };
    use crate::semantics::SearchSemantics;

    fn digest(label: &str) -> DigestV1 {
        DigestV1::hash_bytes(label.as_bytes())
    }

    fn analysis(relative_path: &str) -> AssetAnalysis {
        AssetAnalysis::new(
            AnalyzedSource {
                relative_path: relative_path.to_owned(),
                content_digest: digest(relative_path),
                length: relative_path.len() as u64,
                search_kind: SearchKind::Asset,
                guid: None,
                workspace_source: None,
                workspace_fingerprint: None,
                locator: None,
            },
            SearchFacts::default(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
        )
    }

    fn change_set(
        workspace: WorkspaceId,
        transaction_label: &str,
        from_revision: WorkspaceRevision,
        to_revision: WorkspaceRevision,
        source_local: u128,
    ) -> ChangeSet {
        ChangeSet::new(
            TransactionId::new(digest(transaction_label)),
            workspace,
            from_revision,
            to_revision,
            vec![SourceId::new(workspace, SourceKind::SerializedFile, source_local).unwrap()],
            Vec::new(),
            Vec::new(),
        )
        .unwrap()
    }

    fn source_state(workspace: WorkspaceId, revision: WorkspaceRevision) -> SourceStateSnapshot {
        SourceStateSnapshot::new(
            workspace,
            revision,
            vec![
                SourceScanHint::new("Assets/B.asset".to_owned(), 20, None, Some(10), None).unwrap(),
                SourceScanHint::new("Assets/A.asset".to_owned(), 10, Some(100), None, None)
                    .unwrap(),
            ],
            vec![analysis("Assets/B.asset"), analysis("Assets/A.asset")],
        )
        .unwrap()
    }

    #[test]
    fn source_state_is_canonical_and_self_authenticating() {
        let workspace = WorkspaceId::from_u128(0x51).unwrap();
        let revision = WorkspaceRevision::new(digest("revision"));
        let snapshot = source_state(workspace, revision);

        assert_eq!(snapshot.scan_hints()[0].relative_path, "Assets/A.asset");
        assert_eq!(snapshot.assets()[0].source.relative_path, "Assets/A.asset");
        assert!(snapshot.scan_hint("Assets/B.asset").is_some());
        assert!(snapshot.analysis("Assets/B.asset").is_some());

        let changed_hints = SourceStateSnapshot::new(
            workspace,
            revision,
            vec![
                SourceScanHint::new("Assets/A.asset".to_owned(), 10, Some(999), None, None)
                    .unwrap(),
                SourceScanHint::new("Assets/B.asset".to_owned(), 20, Some(999), Some(10), None)
                    .unwrap(),
            ],
            snapshot.assets().to_vec(),
        )
        .unwrap();
        assert_eq!(changed_hints.logical_digest(), snapshot.logical_digest());

        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let decoded = serde_json::from_slice::<SourceStateSnapshot>(&encoded).unwrap();
        assert_eq!(decoded, snapshot);

        let mut corrupt = serde_json::to_value(&snapshot).unwrap();
        corrupt["logical_digest"] = serde_json::to_value(digest("corrupt")).unwrap();
        assert!(serde_json::from_value::<SourceStateSnapshot>(corrupt).is_err());

        let mut unknown = serde_json::to_value(&snapshot).unwrap();
        unknown["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SourceStateSnapshot>(unknown).is_err());

        for unsupported_version in [
            SOURCE_STATE_CONTRACT_VERSION - 1,
            SOURCE_STATE_CONTRACT_VERSION + 1,
        ] {
            let mut unsupported = serde_json::to_value(&snapshot).unwrap();
            unsupported["contract_version"] = serde_json::json!(unsupported_version);
            assert!(serde_json::from_value::<SourceStateSnapshot>(unsupported).is_err());
        }
    }

    #[test]
    fn source_state_logical_digest_and_manifest_bind_analysis_cache_identity() {
        let workspace = WorkspaceId::from_u128(0x54).unwrap();
        let revision = WorkspaceRevision::new(digest("revision"));
        let configuration_digest = digest("options");
        let current_semantics = SearchSemantics::current();
        let current_identity = current_semantics
            .analysis_cache_identity(configuration_digest)
            .unwrap();
        let current = SourceStateSnapshot::new_with_analysis_cache_identity(
            workspace,
            revision,
            current_identity,
            Vec::new(),
            vec![analysis("Assets/A.asset")],
        )
        .unwrap();
        let stale_semantics = SearchSemantics::current()
            .with_reference_projection_digest(digest("stale reference projection semantics"));
        let stale_identity = stale_semantics
            .analysis_cache_identity(configuration_digest)
            .unwrap();
        let stale = SourceStateSnapshot::new_with_analysis_cache_identity(
            workspace,
            revision,
            stale_identity,
            Vec::new(),
            vec![analysis("Assets/A.asset")],
        )
        .unwrap();
        assert_ne!(current.logical_digest(), stale.logical_digest());

        let manifest = SearchGenerationManifestV1::new(
            SearchGenerationIdentityV1::new_with_semantics(
                workspace,
                revision,
                GenerationProjectionDigests::new(digest("search"), digest("references")),
                Default::default(),
                current_semantics,
                configuration_digest,
                stale.logical_digest(),
            )
            .unwrap(),
            GenerationArtifactEvidence::new(
                ArtifactTreeEvidence::new(digest("search artifacts"), 1, 1),
                ArtifactTreeEvidence::new(digest("reference artifacts"), 1, 1),
                ArtifactTreeEvidence::new(digest("source-state artifacts"), 1, 1),
            ),
        );
        assert!(matches!(
            validate_source_state_manifest(&stale, &manifest),
            Err(SourceStateError::ManifestAnalysisCacheIdentityMismatch {
                expected,
                actual
            }) if expected == current_identity && actual == stale_identity
        ));
    }

    #[test]
    fn json_structure_scan_counts_nested_arrays_members_and_string_backing() {
        let structure = scan_json_structure(br#"{"a":[],"b":[1,{"c":["x","y"]}]}"#).unwrap();

        assert_eq!(structure.array_entries, 4);
        assert_eq!(structure.object_members, 3);
        assert_eq!(structure.string_backing_bytes, 5);
        assert_eq!(structure.max_escaped_string_body_bytes, 0);
        assert_eq!(structure.max_depth, 4);
    }

    #[test]
    fn source_state_allocation_bound_includes_retained_json_escape_scratch() {
        let structure =
            scan_json_structure(br#"{"plain":"abcdefghij","escaped":"abc\n"}"#).unwrap();
        let without_scratch = JsonStructure {
            max_escaped_string_body_bytes: 0,
            ..structure
        };

        assert_eq!(structure.max_escaped_string_body_bytes, 5);
        assert_eq!(
            source_state_owned_allocation_bound(structure).unwrap()
                - source_state_owned_allocation_bound(without_scratch).unwrap(),
            10
        );

        let minimum = scan_json_structure(br#"{"escaped":"\n"}"#).unwrap();
        let minimum_without_scratch = JsonStructure {
            max_escaped_string_body_bytes: 0,
            ..minimum
        };
        assert_eq!(
            source_state_owned_allocation_bound(minimum).unwrap()
                - source_state_owned_allocation_bound(minimum_without_scratch).unwrap(),
            SOURCE_STATE_JSON_SCRATCH_MIN_BYTES
        );

        assert!(matches!(
            source_state_owned_allocation_bound(JsonStructure {
                max_escaped_string_body_bytes: u64::MAX,
                ..minimum
            }),
            Err(SourceStateError::SizeOverflow {
                resource: "source state escaped string scratch"
            })
        ));
    }

    #[test]
    fn transaction_receipts_distinguish_exact_replay_from_id_conflict() {
        let workspace = WorkspaceId::from_u128(0x54).unwrap();
        let from_revision = WorkspaceRevision::new(digest("from"));
        let to_revision = WorkspaceRevision::new(digest("to"));
        let original = change_set(
            workspace,
            "shared transaction",
            from_revision,
            to_revision,
            1,
        );
        let conflict = change_set(
            workspace,
            "shared transaction",
            from_revision,
            to_revision,
            2,
        );
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        let mut receipts =
            TransactionReceiptWindow::from_change_set(&original, &mut budget).unwrap();

        assert!(matches!(
            receipts.membership(&original, &mut budget).unwrap(),
            TransactionReceiptMembership::Exact
        ));
        assert!(matches!(
            receipts.membership(&conflict, &mut budget).unwrap(),
            TransactionReceiptMembership::Conflict
        ));
        assert!(matches!(
            receipts.append(&conflict, &mut budget),
            Err(SourceStateError::TransactionConflict { .. })
        ));
        assert_eq!(receipts.as_slice().len(), 1);
    }

    #[test]
    fn transaction_window_accepts_lagging_receipts_and_appends_from_indexed_revision() {
        let workspace = WorkspaceId::from_u128(0x55).unwrap();
        let revision_0 = WorkspaceRevision::new(digest("revision 0"));
        let revision_1 = WorkspaceRevision::new(digest("revision 1"));
        let revision_2 = WorkspaceRevision::new(digest("revision 2"));
        let revision_3 = WorkspaceRevision::new(digest("revision 3"));
        let first = change_set(workspace, "transaction 1", revision_0, revision_1, 1);
        let second = change_set(workspace, "transaction 2", revision_2, revision_3, 2);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        let receipts = TransactionReceiptWindow::from_change_set(&first, &mut budget).unwrap();

        assert!(matches!(
            receipts.membership(&first, &mut budget).unwrap(),
            TransactionReceiptMembership::Exact
        ));
        let receipts = receipts
            .after_change_set(workspace, revision_2, &second, &mut budget)
            .unwrap();
        assert_eq!(receipts.as_slice().len(), 2);
        assert_eq!(receipts.as_slice()[0].to_revision, revision_1);
        assert_eq!(receipts.as_slice()[1].from_revision, revision_2);
        receipts.validate_for_workspace(workspace).unwrap();
        assert_eq!(receipts.as_slice()[1].to_revision, revision_3);
    }

    #[test]
    fn transaction_receipt_clone_shares_backing_until_append() {
        let workspace = WorkspaceId::from_u128(0x5a).unwrap();
        let revision_0 = WorkspaceRevision::new(digest("shared revision 0"));
        let revision_1 = WorkspaceRevision::new(digest("shared revision 1"));
        let revision_2 = WorkspaceRevision::new(digest("shared revision 2"));
        let first = change_set(workspace, "shared transaction 1", revision_0, revision_1, 1);
        let second = change_set(workspace, "shared transaction 2", revision_1, revision_2, 2);
        let mut budget = AssetLoadBudget::default();
        let original = TransactionReceiptWindow::from_change_set(&first, &mut budget).unwrap();
        let mut appended = original.clone();

        assert!(original.shares_backing_with(&appended));
        appended.append(&second, &mut budget).unwrap();

        assert!(!original.shares_backing_with(&appended));
        assert_eq!(original.as_slice().len(), 1);
        assert_eq!(appended.as_slice().len(), 2);
    }

    #[test]
    fn transaction_window_records_reconciled_receipts_only_at_the_target_revision() {
        let workspace = WorkspaceId::from_u128(0x551).unwrap();
        let revision_0 = WorkspaceRevision::new(digest("reconciled revision 0"));
        let revision_1 = WorkspaceRevision::new(digest("reconciled revision 1"));
        let revision_2 = WorkspaceRevision::new(digest("reconciled revision 2"));
        let changes = change_set(
            workspace,
            "reconciled transaction",
            revision_0,
            revision_1,
            1,
        );
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        let receipts = TransactionReceiptWindow::empty()
            .after_reconciled_target(workspace, revision_1, &changes, &mut budget)
            .unwrap();
        assert_eq!(receipts.as_slice().len(), 1);
        assert_eq!(receipts.as_slice()[0].transaction(), changes.transaction());

        assert!(matches!(
            TransactionReceiptWindow::empty().after_reconciled_target(
                workspace,
                revision_2,
                &changes,
                &mut budget
            ),
            Err(SourceStateError::TransactionReceiptRevisionBarrier { .. })
        ));
    }

    #[test]
    fn transaction_receipt_window_evicts_oldest_without_disabling_new_transactions() {
        let workspace = WorkspaceId::from_u128(0x56).unwrap();
        let revisions = (0..=MAX_TRANSACTION_RECEIPTS + 1)
            .map(|ordinal| WorkspaceRevision::new(DigestV1::hash_bytes(&ordinal.to_le_bytes())))
            .collect::<Vec<_>>();
        let first = change_set(workspace, "transaction-0", revisions[0], revisions[1], 1);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        let mut receipts = TransactionReceiptWindow::from_change_set(&first, &mut budget).unwrap();

        for ordinal in 1..=MAX_TRANSACTION_RECEIPTS {
            let changes = change_set(
                workspace,
                &format!("transaction-{ordinal}"),
                revisions[ordinal],
                revisions[ordinal + 1],
                ordinal as u128 + 1,
            );
            receipts.append(&changes, &mut budget).unwrap();
        }

        assert_eq!(receipts.as_slice().len(), MAX_TRANSACTION_RECEIPTS);
        assert_eq!(
            receipts.as_slice()[0].transaction(),
            TransactionId::new(digest("transaction-1"))
        );
        assert!(matches!(
            receipts.membership(&first, &mut budget).unwrap(),
            TransactionReceiptMembership::Absent
        ));

        assert!(matches!(
            receipts.after_change_set(
                workspace,
                revisions[MAX_TRANSACTION_RECEIPTS + 1],
                &first,
                &mut budget
            ),
            Err(SourceStateError::TransactionReceiptRevisionBarrier { .. })
        ));
    }

    #[test]
    fn source_state_normalizes_all_nested_analysis_collections() {
        let workspace = WorkspaceId::from_u128(0x53).unwrap();
        let revision = WorkspaceRevision::new(digest("revision"));
        let first_address =
            ObjectAddress::binary_direct(SourceLocator::path("Assets/A.asset").unwrap(), 1)
                .unwrap();
        let second_address =
            ObjectAddress::binary_direct(SourceLocator::path("Assets/A.asset").unwrap(), 2)
                .unwrap();
        let mut asset = analysis("Assets/A.asset");
        asset.graph_inputs.objects = vec![
            WorkspaceObjectFact {
                address: second_address.clone(),
                class_id: 2,
                name: None,
            },
            WorkspaceObjectFact {
                address: first_address.clone(),
                class_id: 2,
                name: None,
            },
            WorkspaceObjectFact {
                address: first_address,
                class_id: 1,
                name: None,
            },
        ];
        asset.truncations = vec![
            AnalysisTruncation::new(AnalysisTruncationKind::ContentTerms, Some(10), 11),
            AnalysisTruncation::new(AnalysisTruncationKind::UnityValues, Some(5), 6),
            AnalysisTruncation::new(AnalysisTruncationKind::ContentTerms, Some(10), 11),
        ];

        let snapshot =
            SourceStateSnapshot::new(workspace, revision, Vec::new(), vec![asset]).unwrap();
        let asset = &snapshot.assets()[0];

        assert_eq!(asset.graph_inputs.objects.len(), 2);
        assert_eq!(asset.graph_inputs.objects[0].class_id, 1);
        assert_eq!(asset.graph_inputs.objects[1].address, second_address);
        assert_eq!(asset.truncations.len(), 2);
        assert!(is_strictly_sorted(&asset.truncations));
        assert!(!asset.complete);
        assert!(
            serde_json::from_slice::<SourceStateSnapshot>(&serde_json::to_vec(&snapshot).unwrap())
                .is_ok()
        );
    }

    #[test]
    fn source_state_precharges_dense_short_strings_before_deserializing() {
        let temporary = TempDir::new().unwrap();
        let workspace = WorkspaceId::from_u128(0x56).unwrap();
        let revision = WorkspaceRevision::new(digest("dense strings"));
        let mut value = serde_json::to_value(source_state(workspace, revision)).unwrap();
        value["assets"][0]["search"]["hierarchy_paths"] =
            serde_json::Value::Array(vec![serde_json::Value::String(String::new()); 16_384]);
        let encoded = serde_json::to_vec(&value).unwrap();
        fs::write(temporary.path().join(SOURCE_STATE_FILE), &encoded).unwrap();
        let read_limit = u64::try_from(encoded.len()).unwrap() + 1;
        let structure = scan_json_structure(&encoded).unwrap();
        let owned_allocation = source_state_owned_allocation_bound(structure).unwrap();
        let required = read_limit.checked_add(owned_allocation).unwrap();
        let load_limits = AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(load_limits).unwrap();
        let limits = SourceStateLimits {
            max_encoded_bytes: read_limit,
            ..SourceStateLimits::default()
        };

        assert!(matches!(
            read_source_state_snapshot(temporary.path(), &mut budget, limits),
            Err(GenerationStoreError::Budget(_))
        ));
    }

    #[test]
    fn source_state_precharges_escaped_string_scratch_before_deserializing() {
        let temporary = TempDir::new().unwrap();
        let workspace = WorkspaceId::from_u128(0x58).unwrap();
        let revision = WorkspaceRevision::new(digest("escaped string"));
        let mut value = serde_json::to_value(source_state(workspace, revision)).unwrap();
        value["assets"][0]["search"]["content_terms"] =
            serde_json::Value::String(format!("{}\n", "x".repeat(16_384)));
        let encoded = serde_json::to_vec(&value).unwrap();
        fs::write(temporary.path().join(SOURCE_STATE_FILE), &encoded).unwrap();

        let read_limit = u64::try_from(encoded.len()).unwrap() + 1;
        let structure = scan_json_structure(&encoded).unwrap();
        assert!(structure.max_escaped_string_body_bytes >= 16_386);
        let old_owned_bound = source_state_owned_allocation_bound(JsonStructure {
            max_escaped_string_body_bytes: 0,
            ..structure
        })
        .unwrap();
        let load_limits = AssetLoadLimits {
            max_bytes: read_limit.checked_add(old_owned_bound).unwrap(),
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(load_limits).unwrap();
        let limits = SourceStateLimits {
            max_encoded_bytes: read_limit,
            ..SourceStateLimits::default()
        };

        assert!(matches!(
            read_source_state_snapshot(temporary.path(), &mut budget, limits),
            Err(GenerationStoreError::Budget(_))
        ));
    }

    #[test]
    fn source_state_precharges_duplicate_noncanonical_values_before_deserializing() {
        let temporary = TempDir::new().unwrap();
        let workspace = WorkspaceId::from_u128(0x57).unwrap();
        let revision = WorkspaceRevision::new(digest("duplicate hints"));
        let snapshot = source_state(workspace, revision);
        let mut value = serde_json::to_value(snapshot).unwrap();
        let duplicate = value["scan_hints"][0].clone();
        value["scan_hints"].as_array_mut().unwrap().push(duplicate);
        let encoded = serde_json::to_vec(&value).unwrap();
        fs::write(temporary.path().join(SOURCE_STATE_FILE), &encoded).unwrap();

        let read_limit = u64::try_from(encoded.len()).unwrap() + 1;
        let structure = scan_json_structure(&encoded).unwrap();
        let owned_allocation = source_state_owned_allocation_bound(structure).unwrap();
        let load_limits = AssetLoadLimits {
            max_bytes: read_limit.checked_add(owned_allocation).unwrap() - 1,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(load_limits).unwrap();
        let limits = SourceStateLimits {
            max_encoded_bytes: read_limit,
            ..SourceStateLimits::default()
        };
        assert!(matches!(
            read_source_state_snapshot(temporary.path(), &mut budget, limits),
            Err(GenerationStoreError::Budget(_))
        ));

        let sufficient_limits = AssetLoadLimits {
            max_bytes: read_limit.checked_add(owned_allocation).unwrap(),
            ..AssetLoadLimits::default()
        };
        let mut sufficient_budget = AssetLoadBudget::new(sufficient_limits).unwrap();
        assert!(matches!(
            read_source_state_snapshot(temporary.path(), &mut sufficient_budget, limits),
            Err(GenerationStoreError::SourceState { source, .. })
                if matches!(source.as_ref(), SourceStateError::Json(_))
        ));
    }

    #[test]
    fn source_state_round_trips_through_activated_generation_with_budget() {
        let temporary = TempDir::new().unwrap();
        let workspace = WorkspaceId::from_u128(0x52).unwrap();
        let revision = WorkspaceRevision::new(digest("revision"));
        let snapshot = source_state(workspace, revision);
        let mut store = GenerationStore::open(
            temporary.path(),
            GenerationStoreOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut build = store.begin().unwrap();
        build.write_source_state(&snapshot).unwrap();
        let evidence = store.measure_artifacts(&build).unwrap();
        let identity = SearchGenerationIdentityV1::new(
            workspace,
            revision,
            GenerationProjectionDigests::new(digest("search"), digest("references")),
            Default::default(),
            digest("options"),
            snapshot.logical_digest(),
        )
        .unwrap();
        store
            .prepare_publish(
                &mut build,
                SearchGenerationManifestV1::new(identity, evidence),
            )
            .unwrap()
            .activate()
            .unwrap();

        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        let reopened = store
            .active()
            .unwrap()
            .load_source_state(&mut budget)
            .unwrap();
        assert_eq!(reopened, snapshot);

        let low_limits = AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        };
        let mut low_budget = AssetLoadBudget::new(low_limits).unwrap();
        assert!(matches!(
            store.active().unwrap().load_source_state(&mut low_budget),
            Err(GenerationStoreError::Budget(_))
        ));

        let low_entry_limits = AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        };
        let mut low_entry_budget = AssetLoadBudget::new(low_entry_limits).unwrap();
        assert!(matches!(
            store
                .active()
                .unwrap()
                .load_source_state(&mut low_entry_budget),
            Err(GenerationStoreError::Budget(_))
        ));

        let low_member_limits = AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        };
        let mut low_member_budget = AssetLoadBudget::new(low_member_limits).unwrap();
        assert!(matches!(
            store
                .active()
                .unwrap()
                .load_source_state(&mut low_member_budget),
            Err(GenerationStoreError::Budget(_))
        ));

        let mut entry_budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        let entry_limits = SourceStateLimits {
            max_assets: 1,
            ..SourceStateLimits::default()
        };
        assert!(matches!(
            store
                .active()
                .unwrap()
                .load_source_state_with_limits(&mut entry_budget, entry_limits),
            Err(GenerationStoreError::SourceState { source, .. })
                if matches!(
                    source.as_ref(),
                    SourceStateError::CollectionTooLarge {
                        collection: "assets",
                        ..
                    }
                )
        ));

        fs::write(
            store
                .active()
                .unwrap()
                .source_state_directory()
                .join(SOURCE_STATE_FILE),
            b"{}",
        )
        .unwrap();
        let mut tamper_budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        assert!(matches!(
            store
                .active()
                .unwrap()
                .load_source_state(&mut tamper_budget),
            Err(GenerationStoreError::SourceState { source, .. })
                if matches!(source.as_ref(), SourceStateError::PhysicalEvidenceMismatch { .. })
        ));
    }
}
