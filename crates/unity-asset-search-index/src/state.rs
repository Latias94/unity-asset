use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize};
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, BudgetedJsonError, ChangeSet,
    ContractJsonLimits, ContractJsonResourceModel, Diagnostic, DigestBuildError, DigestV1,
    DigestV1Builder, ObjectAddress, TransactionId, WorkspaceId, WorkspaceRevision,
    read_contract_json,
};
use unity_asset_search_protocol::MAX_PORTABLE_PATH_BYTES;

use crate::analysis::{
    AnalysisTruncation, AssetAnalysis, AssetAnalysisBatch, ContainerEntryFact,
    ReferenceDependencyKey, ReferenceProjectionFact, ReferenceResolutionProjection,
    WorkspaceObjectFact,
};
use crate::generation::{
    ArtifactTreeEvidence, GenerationArtifactEvidence, SearchGenerationId,
    SearchGenerationManifestV1,
};

pub(crate) mod secure_read;

use secure_read::{
    ReadDirectory as SecureReadDirectory, RegularFile as SecureRegularFile, SecureReadError,
};

const GENERATIONS_DIRECTORY: &str = "generations";
const STAGING_DIRECTORY: &str = ".staging";
const ACTIVATIONS_DIRECTORY: &str = "activations";
const SEARCH_ARTIFACT_DIRECTORY: &str = "search";
const REFERENCE_ARTIFACT_DIRECTORY: &str = "references";
const SOURCE_STATE_ARTIFACT_DIRECTORY: &str = "state";
const SOURCE_STATE_FILE: &str = "source-state-v1.json";
const MANIFEST_FILE: &str = "manifest.json";
const LEGACY_ACTIVATION_CONTRACT_VERSION: u16 = 1;
const GENERATION_HEAD_CONTRACT_VERSION: u16 = 2;
const ACTIVATION_FILE_DIGITS: usize = 20;
const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES_U64: u64 = 8 * 1024 * 1024;
const MAX_ACTIVATION_BYTES: usize = 64 * 1024;
const MAX_ACTIVATION_BYTES_U64: u64 = 64 * 1024;
const MAX_ACTIVATION_CANDIDATES: usize = 65_536;
const ACTIVATION_CANDIDATE_GROWTH: usize = 256;
const CONTRACT_JSON_PARSER_WORK_MULTIPLIER: u64 = 6;
const CONTRACT_JSON_PARSER_FIXED_WORK_BYTES: u64 = 4 * 1024;
// A manifest retains at most 4,096 transaction digests. The remaining 128 values cover every
// scalar and nested evidence object in the fixed v1 envelope with room for contract evolution.
const MAX_MANIFEST_JSON_VALUES: u64 = 4_096 + 128;
// The activation record has no variable-size collections. Four KiB plus 256 bytes per observed
// value covers its identifiers, digests, Serde temporaries, and the final fixed-size record.
const ACTIVATION_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    CONTRACT_JSON_PARSER_WORK_MULTIPLIER,
    CONTRACT_JSON_PARSER_FIXED_WORK_BYTES,
    4 * 1024,
    256,
);
const ACTIVATION_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "search.activation",
    MAX_ACTIVATION_BYTES,
    2,
    32,
    32,
    ACTIVATION_JSON_RESOURCES,
);
const MANIFEST_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    CONTRACT_JSON_PARSER_WORK_MULTIPLIER,
    CONTRACT_JSON_PARSER_FIXED_WORK_BYTES,
    // The fixed reserve covers the manifest envelope; 512 bytes per value is deliberately above
    // a retained 32-byte transaction digest and all nested scalar evidence representations.
    64 * 1024,
    512,
);
const MANIFEST_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "search.generation_manifest",
    MAX_MANIFEST_BYTES,
    8,
    MAX_MANIFEST_JSON_VALUES,
    MAX_MANIFEST_JSON_VALUES,
    MANIFEST_JSON_RESOURCES,
);
const MAX_ARTIFACT_RELATIVE_PATH_BYTES: usize = 64 * 1024;
const ARTIFACT_TREE_DOMAIN: &[u8] = b"unity-asset:search-generation:artifact-tree:v1\0";
const MAX_PERSISTED_ARTIFACT_TREE_FILES: u64 = 1_000_000;
const MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES: u64 = 1_000_000;
const MAX_PERSISTED_ARTIFACT_TREE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const SOURCE_STATE_CONTRACT_VERSION: u16 = 1;
const MAX_SOURCE_STATE_ASSETS: usize = 1_000_000;
const MAX_SOURCE_STATE_SCAN_HINTS: usize = 1_000_000;
const MAX_TRANSACTION_RECEIPTS: usize = 4_096;
const MAX_SOURCE_STATE_RELATIVE_PATH_BYTES: usize = MAX_PORTABLE_PATH_BYTES;
// Vec starts with at most eight slots for supported non-zero-sized element types, then grows
// geometrically. Internally tagged Serde enums may temporarily buffer one Content sequence/map
// while constructing the final typed Vec. Two independently grown buffers therefore require at
// most sixteen maximum-sized slots per observed array element or object member.
const SOURCE_STATE_CONTAINER_SLOTS_PER_ITEM: u64 = 16;
// serde_json retains one reusable byte scratch Vec for escaped strings. A byte Vec can begin with
// an eight-byte allocation and geometric growth stays below twice the requested length.
const SOURCE_STATE_JSON_SCRATCH_MIN_BYTES: u64 = 8;
const WRITER_LEASE_FILE: &str = ".writer.lock";
const QUARANTINE_DIRECTORY_PREFIX: &str = "quarantine-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenerationStoreOptions {
    pub retain_previous_generations: usize,
}

impl Default for GenerationStoreOptions {
    fn default() -> Self {
        Self {
            retain_previous_generations: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceStateLimits {
    pub(crate) max_encoded_bytes: u64,
    pub(crate) max_assets: usize,
    pub(crate) max_scan_hints: usize,
    pub(crate) max_transaction_receipts: usize,
    pub(crate) max_relative_path_bytes: usize,
}

impl Default for SourceStateLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 512 * 1024 * 1024,
            max_assets: MAX_SOURCE_STATE_ASSETS,
            max_scan_hints: MAX_SOURCE_STATE_SCAN_HINTS,
            max_transaction_receipts: MAX_TRANSACTION_RECEIPTS,
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
            contract_version: SOURCE_STATE_CONTRACT_VERSION,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TransactionReceiptWindow {
    receipts: Vec<TransactionReceipt>,
}

impl TransactionReceiptWindow {
    #[must_use]
    pub(crate) const fn empty() -> Self {
        Self {
            receipts: Vec::new(),
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

    pub(crate) fn ids(&self) -> impl ExactSizeIterator<Item = TransactionId> + '_ {
        self.receipts.iter().map(|receipt| receipt.transaction())
    }

    pub(crate) fn canonical_ids(&self) -> Vec<TransactionId> {
        let mut transactions = self.ids().collect::<Vec<_>>();
        transactions.sort_unstable();
        transactions
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
        let incoming = TransactionReceipt::from_change_set(changes, budget)?;
        Ok(
            match self
                .receipts
                .iter()
                .find(|receipt| receipt.transaction == incoming.transaction)
                .copied()
            {
                Some(existing) if existing == incoming => TransactionReceiptMembership::Exact,
                Some(existing) => TransactionReceiptMembership::Conflict { existing, incoming },
                None => TransactionReceiptMembership::Absent { incoming },
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
        let incoming = match self.membership(changes, budget)? {
            TransactionReceiptMembership::Exact => return Ok(()),
            TransactionReceiptMembership::Conflict { existing, incoming } => {
                return Err(SourceStateError::TransactionConflict {
                    existing: Box::new(existing),
                    incoming: Box::new(incoming),
                });
            }
            TransactionReceiptMembership::Absent { incoming } => incoming,
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
        if self.receipts.len() == MAX_TRANSACTION_RECEIPTS {
            self.receipts.remove(0);
        } else {
            self.receipts.try_reserve_exact(1).map_err(|error| {
                SourceStateError::AllocationFailed {
                    requested: receipt_bytes as usize,
                    message: error.to_string(),
                }
            })?;
        }
        self.receipts.push(incoming);
        Ok(())
    }

    pub(crate) fn try_clone_with_budget(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SourceStateError> {
        let entries =
            u64::try_from(self.receipts.len()).map_err(|_| SourceStateError::SizeOverflow {
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
            .try_reserve_exact(self.receipts.len())
            .map_err(|error| SourceStateError::AllocationFailed {
                requested: self
                    .receipts
                    .len()
                    .saturating_mul(std::mem::size_of::<TransactionReceipt>()),
                message: error.to_string(),
            })?;
        receipts.extend_from_slice(&self.receipts);
        Ok(Self { receipts })
    }

    fn validate(&self, workspace: WorkspaceId, maximum: usize) -> Result<(), SourceStateError> {
        validate_source_state_count("transaction receipts", self.receipts.len(), maximum)?;
        for (index, receipt) in self.receipts.iter().enumerate() {
            if receipt.contract_version != SOURCE_STATE_CONTRACT_VERSION {
                return Err(SourceStateError::UnsupportedTransactionReceiptVersion {
                    actual: receipt.contract_version,
                    expected: SOURCE_STATE_CONTRACT_VERSION,
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
    transaction_receipts: TransactionReceiptWindow,
    scan_hints: Vec<SourceScanHint>,
    assets: Vec<AssetAnalysis>,
    logical_digest: DigestV1,
}

impl SourceStateSnapshot {
    pub(crate) fn from_batch(
        batch: AssetAnalysisBatch,
        scan_hints: Vec<SourceScanHint>,
        transaction_receipts: TransactionReceiptWindow,
    ) -> Result<Self, SourceStateError> {
        if !transaction_receipts.matches_canonical_ids(&batch.transactions) {
            return Err(SourceStateError::BatchTransactionsMismatch {
                batch: batch.transactions,
                receipts: transaction_receipts.canonical_ids(),
            });
        }
        Self::new(
            batch.workspace,
            batch.revision,
            transaction_receipts,
            scan_hints,
            batch.assets,
        )
    }

    pub(crate) fn new(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        transaction_receipts: TransactionReceiptWindow,
        mut scan_hints: Vec<SourceScanHint>,
        mut assets: Vec<AssetAnalysis>,
    ) -> Result<Self, SourceStateError> {
        transaction_receipts.validate(workspace, MAX_TRANSACTION_RECEIPTS)?;
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
            source_state_logical_digest(workspace, revision, &transaction_receipts, &assets)?;
        Ok(Self {
            contract_version: SOURCE_STATE_CONTRACT_VERSION,
            workspace,
            revision,
            transaction_receipts,
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
    pub(crate) const fn transaction_receipts(&self) -> &TransactionReceiptWindow {
        &self.transaction_receipts
    }

    pub(crate) fn transaction_membership(
        &self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<TransactionReceiptMembership, SourceStateError> {
        self.transaction_receipts.membership(changes, budget)
    }

    /// Extends the receipt window only from this snapshot's revision.
    ///
    /// Receipt history may lag this snapshot after filesystem reconciliation. The revision check
    /// belongs here instead of in `TransactionReceiptWindow`, where digest values cannot express
    /// a chronology.
    pub(crate) fn transaction_receipts_after(
        &self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<TransactionReceiptWindow, SourceStateError> {
        if self.workspace != changes.workspace() {
            return Err(SourceStateError::TransactionReceiptWorkspaceMismatch {
                expected: self.workspace,
                actual: changes.workspace(),
                transaction: changes.transaction(),
            });
        }
        if self.revision != changes.from_revision() {
            return Err(SourceStateError::TransactionReceiptRevisionBarrier {
                indexed: self.revision,
                change_from: changes.from_revision(),
                change_to: changes.to_revision(),
            });
        }
        let mut receipts = self.transaction_receipts.try_clone_with_budget(budget)?;
        receipts.append(changes, budget)?;
        Ok(receipts)
    }

    /// Extends the receipt window when filesystem reconciliation already observed the target.
    ///
    /// This is the only legal receipt-only transition: the source state must already represent
    /// the Change Set's target revision, while the caller separately proves the matching
    /// `WorkspaceView` revision before publication.
    pub(crate) fn transaction_receipts_after_reconciled_target(
        &self,
        changes: &ChangeSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<TransactionReceiptWindow, SourceStateError> {
        if self.workspace != changes.workspace() {
            return Err(SourceStateError::TransactionReceiptWorkspaceMismatch {
                expected: self.workspace,
                actual: changes.workspace(),
                transaction: changes.transaction(),
            });
        }
        if self.revision != changes.to_revision() {
            return Err(SourceStateError::TransactionReceiptRevisionBarrier {
                indexed: self.revision,
                change_from: changes.from_revision(),
                change_to: changes.to_revision(),
            });
        }
        let mut receipts = self.transaction_receipts.try_clone_with_budget(budget)?;
        receipts.append(changes, budget)?;
        Ok(receipts)
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

    fn validate_limits(&self, limits: SourceStateLimits) -> Result<(), SourceStateError> {
        self.transaction_receipts
            .validate(self.workspace, limits.max_transaction_receipts)?;
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
    transaction_receipts: TransactionReceiptWindow,
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
        let snapshot = Self::new(
            wire.workspace,
            wire.revision,
            wire.transaction_receipts,
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
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    transaction_receipts: &'state TransactionReceiptWindow,
    assets: &'state [AssetAnalysis],
}

fn source_state_logical_digest(
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    transaction_receipts: &TransactionReceiptWindow,
    assets: &[AssetAnalysis],
) -> Result<DigestV1, SourceStateError> {
    let logical = SourceStateLogicalRef {
        contract_version: SOURCE_STATE_CONTRACT_VERSION,
        workspace,
        revision,
        transaction_receipts,
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
struct ByteCounter {
    bytes: u64,
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

struct SizeLimitedWriter<W> {
    inner: W,
    written: u64,
    maximum: u64,
    rejected_bytes: Option<u64>,
}

impl<W> SizeLimitedWriter<W> {
    const fn new(inner: W, maximum: u64) -> Self {
        Self {
            inner,
            written: 0,
            maximum,
            rejected_bytes: None,
        }
    }

    const fn rejected_bytes(&self) -> Option<u64> {
        self.rejected_bytes
    }

    const fn inner(&self) -> &W {
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

#[derive(Debug, Default)]
struct LiveBuildClaim {
    held: AtomicBool,
}

impl LiveBuildClaim {
    fn acquire(self: &Arc<Self>) -> Result<LiveBuildToken, GenerationStoreError> {
        self.held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| GenerationStoreError::BuildAlreadyActive)?;
        Ok(LiveBuildToken {
            claim: Arc::clone(self),
            held: true,
        })
    }

    fn is_held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct LiveBuildToken {
    claim: Arc<LiveBuildClaim>,
    held: bool,
}

impl LiveBuildToken {
    fn belongs_to(&self, claim: &Arc<LiveBuildClaim>) -> bool {
        Arc::ptr_eq(&self.claim, claim)
    }

    fn release(&mut self) {
        if self.held {
            self.claim.held.store(false, Ordering::Release);
            self.held = false;
        }
    }
}

impl Drop for LiveBuildToken {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationBuildState {
    Armed,
    Completed,
    Relinquished,
}

/// A store-owned staging directory. All writable paths are derived from its ordinal.
#[derive(Debug)]
pub(crate) struct GenerationBuild {
    store_root: PathBuf,
    ordinal: u64,
    directory: PathBuf,
    lease: Arc<WriterLease>,
    claim: LiveBuildToken,
    state: GenerationBuildState,
}

impl GenerationBuild {
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[cfg(test)]
    #[must_use]
    pub fn search_directory(&self) -> PathBuf {
        self.directory.join(SEARCH_ARTIFACT_DIRECTORY)
    }

    #[cfg(test)]
    #[must_use]
    pub fn reference_directory(&self) -> PathBuf {
        self.directory.join(REFERENCE_ARTIFACT_DIRECTORY)
    }

    #[must_use]
    pub fn source_state_directory(&self) -> PathBuf {
        self.directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY)
    }

    pub(crate) fn abort_with_budget(
        &mut self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError> {
        if self.state != GenerationBuildState::Armed {
            return Ok(());
        }
        let result = self.cleanup_directory_with_budget(budget);
        self.state = GenerationBuildState::Relinquished;
        self.claim.release();
        result
    }

    pub(crate) fn write_source_state(
        &self,
        snapshot: &SourceStateSnapshot,
        limits: SourceStateLimits,
    ) -> Result<(), SourceStateError> {
        snapshot.validate_limits(limits)?;
        let directory = self.source_state_directory();
        ensure_existing_directory_no_follow(&directory).map_err(SourceStateError::store)?;
        let path = directory.join(SOURCE_STATE_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| {
                SourceStateError::store(GenerationStoreError::io(
                    "create source state",
                    path.clone(),
                    source,
                ))
            })?;
        let mut writer = SizeLimitedWriter::new(BufWriter::new(file), limits.max_encoded_bytes);
        let encoded = serde_json::to_writer(&mut writer, snapshot);
        if let Some(actual) = writer.rejected_bytes() {
            return Err(SourceStateError::EncodedTooLarge {
                actual,
                maximum: limits.max_encoded_bytes,
            });
        }
        encoded.map_err(SourceStateError::Json)?;
        writer.flush().map_err(|source| {
            SourceStateError::store(GenerationStoreError::io(
                "flush source state",
                path.clone(),
                source,
            ))
        })?;
        writer.inner().get_ref().sync_all().map_err(|source| {
            SourceStateError::store(GenerationStoreError::io("sync source state", path, source))
        })
    }

    fn cleanup_directory_with_budget(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError> {
        if path_exists_no_follow(&self.directory)? {
            remove_tree_no_follow(&self.directory, budget)?;
            sync_directory(
                self.directory
                    .parent()
                    .ok_or(GenerationStoreError::ForeignBuild)?,
            )?;
        }
        Ok(())
    }

    fn mark_completed(&mut self) {
        debug_assert_eq!(self.state, GenerationBuildState::Armed);
        self.state = GenerationBuildState::Completed;
        self.claim.release();
    }
}

impl Drop for GenerationBuild {
    fn drop(&mut self) {
        if self.state == GenerationBuildState::Armed {
            let _ = self.cleanup_directory_with_budget(&mut AssetLoadBudget::default());
            self.state = GenerationBuildState::Relinquished;
        }
        self.claim.release();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationStagingState {
    Armed,
    Committed,
    Relinquished,
}

/// Owns one activation staging file from creation through commit or explicit cleanup.
#[derive(Debug)]
struct ActivationStagingFile {
    path: PathBuf,
    parent: PathBuf,
    state: ActivationStagingState,
}

impl ActivationStagingFile {
    fn create(path: PathBuf, bytes: &[u8]) -> Result<Self, GenerationStoreError> {
        let parent = path
            .parent()
            .ok_or(GenerationStoreError::ForeignBuild)?
            .to_path_buf();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| {
                GenerationStoreError::io("create activation staging file", path.clone(), source)
            })?;
        let mut staging = Self {
            path,
            parent,
            state: ActivationStagingState::Armed,
        };
        let write_result = file
            .write_all(bytes)
            .map_err(|source| {
                GenerationStoreError::io(
                    "write activation staging file",
                    staging.path.clone(),
                    source,
                )
            })
            .and_then(|()| {
                file.sync_all().map_err(|source| {
                    GenerationStoreError::io(
                        "sync activation staging file",
                        staging.path.clone(),
                        source,
                    )
                })
            });
        drop(file);
        match write_result {
            Ok(()) => Ok(staging),
            Err(primary) => Err(staging.precommit_failure(primary)),
        }
    }

    fn publish(
        &mut self,
        final_path: &Path,
        failpoint: Option<GenerationFailpoint>,
    ) -> Result<(), GenerationStoreError> {
        debug_assert_eq!(self.state, ActivationStagingState::Armed);
        let publish_result = inject_failure(failpoint, GenerationFailpoint::ActivationPreCommit)
            .and_then(|()| {
                fs::hard_link(&self.path, final_path).map_err(|source| {
                    GenerationStoreError::io(
                        "activate generation",
                        final_path.to_path_buf(),
                        source,
                    )
                })
            });
        match publish_result {
            Ok(()) => {
                self.state = ActivationStagingState::Committed;
                Ok(())
            }
            Err(primary) => Err(self.precommit_failure(primary)),
        }
    }

    fn cleanup_after_commit(&mut self) -> Result<(), GenerationStoreError> {
        debug_assert_eq!(self.state, ActivationStagingState::Committed);
        self.cleanup_and_relinquish("remove committed activation staging file")
    }

    fn precommit_failure(&mut self, primary: GenerationStoreError) -> GenerationStoreError {
        match self.cleanup_and_relinquish("remove uncommitted activation staging file") {
            Ok(()) => primary,
            Err(cleanup) => GenerationStoreError::ActivationPreCommitCleanupFailed {
                primary: Box::new(primary),
                cleanup: Box::new(cleanup),
            },
        }
    }

    fn cleanup_and_relinquish(
        &mut self,
        operation: &'static str,
    ) -> Result<(), GenerationStoreError> {
        if self.state == ActivationStagingState::Relinquished {
            return Ok(());
        }
        let result = fs::remove_file(&self.path)
            .map_err(|source| GenerationStoreError::io(operation, self.path.clone(), source))
            .and_then(|()| sync_directory(&self.parent));
        self.state = ActivationStagingState::Relinquished;
        result
    }
}

impl Drop for ActivationStagingFile {
    fn drop(&mut self) {
        if self.state != ActivationStagingState::Relinquished {
            let _ = self.cleanup_and_relinquish("remove dropped activation staging file");
        }
    }
}

/// Immutable view of one activated generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationSnapshot {
    activation_ordinal: u64,
    generation: SearchGenerationId,
    manifest_digest: DigestV1,
    manifest: SearchGenerationManifestV1,
    desired_revision: WorkspaceRevision,
    directory: PathBuf,
}

impl GenerationSnapshot {
    #[must_use]
    pub const fn activation_ordinal(&self) -> u64 {
        self.activation_ordinal
    }

    #[must_use]
    pub const fn generation(&self) -> SearchGenerationId {
        self.generation
    }

    #[must_use]
    pub const fn desired_revision(&self) -> WorkspaceRevision {
        self.desired_revision
    }

    #[must_use]
    pub const fn manifest(&self) -> &SearchGenerationManifestV1 {
        &self.manifest
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[cfg(test)]
    #[must_use]
    pub fn search_directory(&self) -> PathBuf {
        self.directory.join(SEARCH_ARTIFACT_DIRECTORY)
    }

    #[must_use]
    pub fn source_state_directory(&self) -> PathBuf {
        self.directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY)
    }

    pub(crate) fn load_source_state(
        &self,
        budget: &mut AssetLoadBudget,
        limits: SourceStateLimits,
    ) -> Result<SourceStateSnapshot, SourceStateError> {
        let directory = self.source_state_directory();
        let actual = measure_artifact_tree(&directory).map_err(SourceStateError::store)?;
        let expected = self.manifest.artifacts().source_state();
        if actual != expected {
            return Err(SourceStateError::PhysicalEvidenceMismatch { expected, actual });
        }
        let snapshot = read_source_state_snapshot(&directory, budget, limits)?;
        validate_source_state_manifest(&snapshot, &self.manifest)?;
        Ok(snapshot)
    }
}

fn read_source_state_snapshot(
    directory: &Path,
    budget: &mut AssetLoadBudget,
    limits: SourceStateLimits,
) -> Result<SourceStateSnapshot, SourceStateError> {
    let opened_directory = SecureReadDirectory::open(directory).map_err(|source| {
        SourceStateError::store(persisted_read_error(
            "open source-state directory",
            directory.to_path_buf(),
            source,
        ))
    })?;
    read_source_state_snapshot_in(&opened_directory, directory, budget, limits)
}

fn read_source_state_snapshot_in(
    directory: &SecureReadDirectory,
    directory_path: &Path,
    budget: &mut AssetLoadBudget,
    limits: SourceStateLimits,
) -> Result<SourceStateSnapshot, SourceStateError> {
    let path = directory_path.join(SOURCE_STATE_FILE);
    let mut file = directory
        .open_regular(OsStr::new(SOURCE_STATE_FILE))
        .map_err(|source| {
            SourceStateError::store(persisted_read_error(
                "open source state",
                path.clone(),
                source,
            ))
        })?;
    let encoded_length = file.length();
    if encoded_length > limits.max_encoded_bytes {
        return Err(SourceStateError::EncodedTooLarge {
            actual: encoded_length,
            maximum: limits.max_encoded_bytes,
        });
    }

    let read_limit = encoded_length
        .checked_add(1)
        .ok_or(SourceStateError::SizeOverflow {
            resource: "source state read limit",
        })?;
    budget
        .check_bytes(read_limit)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .consume_bytes(read_limit)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    let capacity = usize::try_from(read_limit).map_err(|_| SourceStateError::SizeOverflow {
        resource: "source state read buffer",
    })?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|error| SourceStateError::AllocationFailed {
            requested: capacity,
            message: error.to_string(),
        })?;
    Read::by_ref(file.file_mut())
        .take(read_limit)
        .read_to_end(&mut encoded)
        .map_err(|source| {
            SourceStateError::store(GenerationStoreError::io(
                "read source state",
                path.clone(),
                source,
            ))
        })?;
    file.ensure_unchanged().map_err(|source| {
        SourceStateError::store(persisted_read_error(
            "revalidate source state",
            path.clone(),
            source,
        ))
    })?;
    let actual = u64::try_from(encoded.len()).map_err(|_| SourceStateError::SizeOverflow {
        resource: "source state encoded length",
    })?;
    if actual > limits.max_encoded_bytes {
        return Err(SourceStateError::EncodedTooLarge {
            actual,
            maximum: limits.max_encoded_bytes,
        });
    }
    if actual != encoded_length {
        return Err(SourceStateError::EncodedLengthChanged {
            expected: encoded_length,
            actual,
        });
    }

    let structure = scan_json_structure(&encoded)?;
    let owned_allocation = source_state_owned_allocation_bound(structure)?;
    budget
        .check_entries(structure.array_entries)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .check_members(structure.object_members)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .check_depth(structure.max_depth)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .check_bytes(owned_allocation)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .consume_entries(structure.array_entries)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .consume_members(structure.object_members)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .observe_depth(structure.max_depth)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    budget
        .consume_bytes(owned_allocation)
        .map_err(|source| SourceStateError::Budget(BudgetedJsonError::Budget(source)))?;
    let snapshot: SourceStateSnapshot =
        serde_json::from_slice(&encoded).map_err(SourceStateError::Json)?;
    snapshot.validate_limits(limits)?;
    let semantic_entries = source_state_entry_count(&snapshot)?;
    if semantic_entries > structure.array_entries {
        return Err(SourceStateError::StructuralEntryUnderestimate {
            structural: structure.array_entries,
            semantic: semantic_entries,
        });
    }
    Ok(snapshot)
}

fn validate_source_state_manifest(
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
    if !snapshot
        .transaction_receipts
        .matches_canonical_ids(manifest.applied_transactions())
    {
        return Err(SourceStateError::ManifestTransactionsMismatch);
    }
    Ok(())
}

fn validate_persisted_source_state(
    directory: &Path,
    manifest: &SearchGenerationManifestV1,
) -> Result<(), GenerationStoreError> {
    let source_state_directory = directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY);
    let opened_source_state =
        SecureReadDirectory::open(&source_state_directory).map_err(|source| {
            persisted_read_error(
                "open completed source-state directory",
                source_state_directory.clone(),
                source,
            )
        })?;
    validate_persisted_source_state_in(
        directory,
        &source_state_directory,
        &opened_source_state,
        manifest,
    )
}

fn validate_persisted_source_state_in(
    generation_directory: &Path,
    source_state_directory: &Path,
    opened_source_state: &SecureReadDirectory,
    manifest: &SearchGenerationManifestV1,
) -> Result<(), GenerationStoreError> {
    let source_limits = SourceStateLimits::default();
    let mut load_limits = AssetLoadLimits::default();
    let validation_read_limit = source_limits.max_encoded_bytes.checked_add(1).ok_or(
        GenerationStoreError::SizeOverflow {
            resource: "source state validation read limit",
        },
    )?;
    load_limits.max_entries = validation_read_limit;
    load_limits.max_members = validation_read_limit;
    load_limits.max_depth = 64;
    let mut budget = AssetLoadBudget::new(load_limits).map_err(|source| {
        invalid_source_state(
            generation_directory,
            SourceStateError::Budget(BudgetedJsonError::Budget(source)),
        )
    })?;
    let snapshot = read_source_state_snapshot_in(
        opened_source_state,
        source_state_directory,
        &mut budget,
        source_limits,
    )
    .map_err(|error| classify_persisted_source_state_error(generation_directory, error))?;
    validate_source_state_manifest(&snapshot, manifest)
        .map_err(|error| classify_persisted_source_state_error(generation_directory, error))
}

fn source_state_entry_count(snapshot: &SourceStateSnapshot) -> Result<u64, SourceStateError> {
    let mut entries = snapshot
        .transaction_receipts
        .receipts
        .len()
        .checked_add(snapshot.scan_hints.len())
        .and_then(|count| count.checked_add(snapshot.assets.len()))
        .ok_or(SourceStateError::SizeOverflow {
            resource: "source state entries",
        })?;
    for analysis in &snapshot.assets {
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
struct JsonStructure {
    array_entries: u64,
    object_members: u64,
    // JSON escape syntax never decodes to more UTF-8 bytes than its raw string body.
    string_backing_bytes: u64,
    max_escaped_string_body_bytes: u64,
    max_depth: u32,
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

fn scan_json_structure(encoded: &[u8]) -> Result<JsonStructure, SourceStateError> {
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

fn source_state_owned_allocation_bound(structure: JsonStructure) -> Result<u64, SourceStateError> {
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

fn invalid_source_state(directory: &Path, source: SourceStateError) -> GenerationStoreError {
    GenerationStoreError::InvalidSourceState {
        path: directory
            .join(SOURCE_STATE_ARTIFACT_DIRECTORY)
            .join(SOURCE_STATE_FILE),
        message: source.to_string(),
    }
}

fn classify_persisted_source_state_error(
    directory: &Path,
    source: SourceStateError,
) -> GenerationStoreError {
    match source {
        SourceStateError::Store(source) if source.is_security_violation() => *source,
        SourceStateError::Budget(source) => GenerationStoreError::ContractJson {
            artifact: "source state",
            path: directory
                .join(SOURCE_STATE_ARTIFACT_DIRECTORY)
                .join(SOURCE_STATE_FILE),
            source,
        },
        SourceStateError::AllocationFailed { requested, .. } => {
            GenerationStoreError::AllocationFailed {
                resource: "source state materialization",
                requested,
            }
        }
        SourceStateError::SizeOverflow { resource } => {
            GenerationStoreError::SizeOverflow { resource }
        }
        source => invalid_source_state(directory, source),
    }
}

/// Classifies non-fatal work that could not be completed around generation publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationPublishWarningKind {
    /// Cleanup of an inactive completed-generation preparation could not be finished.
    PreparationCleanup,
    /// The activation is visible, but crash durability could not be confirmed.
    PostCommitDurability,
    /// The activation is visible, but its staging cleanup could not be finished durably.
    PostCommitCleanup,
    /// Best-effort retention after selecting the active generation could not be finished.
    Retention,
}

/// Typed evidence for non-fatal generation publication work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationPublishWarning {
    kind: GenerationPublishWarningKind,
    message: String,
}

impl GenerationPublishWarning {
    pub(crate) fn new(kind: GenerationPublishWarningKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> GenerationPublishWarningKind {
        self.kind
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for GenerationPublishWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn platform_durability_warnings() -> Vec<GenerationPublishWarning> {
    #[cfg(unix)]
    {
        Vec::new()
    }
    #[cfg(not(unix))]
    {
        vec![GenerationPublishWarning::new(
            GenerationPublishWarningKind::PostCommitDurability,
            "directory namespace durability is platform-dependent; file contents and create-new activation were synced, but the standard library cannot fsync directories on this platform",
        )]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationPublishReport {
    pub active: GenerationSnapshot,
    pub pruned_generations: Vec<SearchGenerationId>,
    pub warnings: Vec<GenerationPublishWarning>,
}

/// A completed generation whose readers must be validated before activation.
///
/// Holding this capability keeps the store mutably borrowed, so no second generation can be
/// prepared or activated out of order. Dropping it leaves a valid, inactive completed generation
/// that recovery can safely reuse.
#[derive(Debug)]
pub(crate) struct PreparedGenerationPublish<'store> {
    store: &'store mut GenerationStore,
    snapshot: GenerationSnapshot,
    activation: PreparedActivation,
    warnings: Vec<GenerationPublishWarning>,
    failpoint: Option<GenerationFailpoint>,
}

impl PreparedGenerationPublish<'_> {
    #[must_use]
    pub const fn snapshot(&self) -> &GenerationSnapshot {
        &self.snapshot
    }

    /// Durably activates the generation after the caller has opened and validated every reader
    /// represented by [`Self::snapshot`].
    ///
    /// Reader validation is intentionally absent here. Once the activation hard link is visible,
    /// later durability, cleanup, and retention failures are returned as typed warnings so a
    /// successful result always agrees with both in-memory and reopened active state.
    #[cfg(test)]
    pub fn activate(self) -> Result<GenerationPublishReport, GenerationStoreError> {
        self.activate_with_budget(&mut AssetLoadBudget::default())
    }

    pub fn activate_with_budget(
        self,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublishReport, GenerationStoreError> {
        let Self {
            store,
            snapshot,
            activation,
            mut warnings,
            failpoint,
        } = self;
        match activation {
            PreparedActivation::AlreadyActive => {
                let mut maintenance_budget = AssetLoadBudget::default();
                let pruned_generations = match store.prune_retention(&mut maintenance_budget) {
                    Ok(pruned) => pruned,
                    Err(error) => {
                        warnings.push(GenerationPublishWarning::new(
                            GenerationPublishWarningKind::Retention,
                            error.to_string(),
                        ));
                        Vec::new()
                    }
                };
                Ok(GenerationPublishReport {
                    active: snapshot,
                    pruned_generations,
                    warnings,
                })
            }
            PreparedActivation::Activate { manifest_digest } => {
                inject_failure(failpoint, GenerationFailpoint::Activation)?;
                store.activate_prepared(snapshot, manifest_digest, warnings, failpoint, budget)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedActivation {
    AlreadyActive,
    Activate { manifest_digest: DigestV1 },
}

/// Preflight estimate for the period where the old and new generations coexist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenerationDiskEstimate {
    pub existing_generation_bytes: u64,
    pub old_active_generation_bytes: u64,
    pub new_generation_bytes: u64,
    pub publish_peak_bytes: u64,
    pub retained_bytes_after_publish: u64,
    pub reclaimable_bytes_after_publish: u64,
}

/// Deterministic failure injection checkpoints used by state-machine tests.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationFailpoint {
    Search,
    References,
    SourceState,
    Activation,
    ActivationPreCommit,
    ActivationDirectorySync,
    ActivationCleanup,
}

#[derive(Debug)]
struct WriterLease {
    file: File,
}

impl WriterLease {
    fn acquire(root: &Path) -> Result<Self, GenerationStoreError> {
        let path = root.join(WRITER_LEASE_FILE);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path).map_err(|source| {
                    GenerationStoreError::io(
                        "inspect generation writer lease",
                        path.clone(),
                        source,
                    )
                })?;
                reject_link_or_reparse(&path, &metadata)?;
                if !metadata.is_file() {
                    return Err(GenerationStoreError::UnsupportedFileType { path });
                }
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|source| {
                        GenerationStoreError::io(
                            "open generation writer lease",
                            path.clone(),
                            source,
                        )
                    })?
            }
            Err(source) => {
                return Err(GenerationStoreError::io(
                    "create generation writer lease",
                    path,
                    source,
                ));
            }
        };
        let metadata = fs::symlink_metadata(&path).map_err(|source| {
            GenerationStoreError::io("reinspect generation writer lease", path.clone(), source)
        })?;
        reject_link_or_reparse(&path, &metadata)?;
        if !metadata.is_file() {
            return Err(GenerationStoreError::UnsupportedFileType { path });
        }
        file.try_lock_exclusive()
            .map_err(|source| GenerationStoreError::WriterLeaseUnavailable { path, source })?;
        Ok(Self { file })
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Bounded evidence produced by one abandoned-staging reconciliation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GenerationStagingRecoveryReport {
    removed_entries: u64,
}

impl GenerationStagingRecoveryReport {
    #[must_use]
    pub(crate) const fn removed_entries(self) -> u64 {
        self.removed_entries
    }
}

#[derive(Debug)]
pub(crate) struct GenerationStore {
    root: PathBuf,
    generations: PathBuf,
    staging: PathBuf,
    activations: PathBuf,
    options: GenerationStoreOptions,
    active: Option<GenerationSnapshot>,
    next_staging_ordinal: u64,
    next_activation_ordinal: u64,
    lease: Arc<WriterLease>,
    live_build: Arc<LiveBuildClaim>,
}

impl GenerationStore {
    /// Opens the durable store and selects the generation named by the latest committed head.
    ///
    /// The highest head is the sole authority for actual and desired revision state. Corruption in
    /// that head or its immutable generation therefore fails closed instead of silently rolling
    /// freshness back to an older activation. Directory discovery and validation share the
    /// caller's ledger.
    pub fn open(
        root: impl AsRef<Path>,
        options: GenerationStoreOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, GenerationStoreError> {
        let root = initialize_root(root.as_ref())?;
        let lease = Arc::new(WriterLease::acquire(&root)?);
        let generations = ensure_managed_directory(&root, GENERATIONS_DIRECTORY)?;
        let staging = ensure_managed_directory(&root, STAGING_DIRECTORY)?;
        let activations = ensure_managed_directory(&root, ACTIVATIONS_DIRECTORY)?;
        let opened_generations = SecureReadDirectory::open(&generations).map_err(|source| {
            persisted_read_error("open generations directory", generations.clone(), source)
        })?;
        let opened_activations = SecureReadDirectory::open(&activations).map_err(|source| {
            persisted_read_error("open activations directory", activations.clone(), source)
        })?;
        recover_owned_staging(&staging, budget)?;

        let next_staging_ordinal = next_staging_ordinal(&staging, budget)?;
        let (activation_candidates, next_activation_ordinal) =
            activation_candidates_for_open(&activations, &staging, budget)?;
        let active = select_active_generation(
            &generations,
            &opened_generations,
            &opened_activations,
            &activation_candidates,
            budget,
        )?;

        Ok(Self {
            root,
            generations,
            staging,
            activations,
            options,
            active,
            next_staging_ordinal,
            next_activation_ordinal,
            lease,
            live_build: Arc::new(LiveBuildClaim::default()),
        })
    }

    #[must_use]
    pub const fn active(&self) -> Option<&GenerationSnapshot> {
        self.active.as_ref()
    }

    /// Removes only store-owned abandoned entries from the private staging namespace.
    ///
    /// A live build is an exclusive capability. Recovery refuses to run while that capability is
    /// armed, so it cannot mistake an in-flight build for crash residue.
    pub(crate) fn reconcile_abandoned_staging(
        &mut self,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationStagingRecoveryReport, GenerationStoreError> {
        if self.live_build.is_held() {
            return Err(GenerationStoreError::BuildAlreadyActive);
        }
        recover_owned_staging(&self.staging, budget)
    }

    /// Appends a durable head for the current immutable generation before derived work starts.
    ///
    /// The hard-linked head is the commit point. A later generation activation records
    /// `desired_revision == actual_revision`, so no mutable sidecar or cross-file clearing
    /// protocol is required.
    pub fn record_desired_revision(
        &mut self,
        desired_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<GenerationPublishWarning>, GenerationStoreError> {
        let Some(active) = self.active.clone() else {
            return Ok(Vec::new());
        };
        if active.desired_revision == desired_revision {
            return Ok(Vec::new());
        }

        let mut updated = active;
        updated.activation_ordinal = self.allocate_activation_ordinal()?;
        updated.desired_revision = desired_revision;
        let report = self.activate_prepared(
            updated.clone(),
            updated.manifest_digest,
            platform_durability_warnings(),
            None,
            budget,
        )?;
        Ok(report.warnings)
    }

    #[must_use]
    pub fn generation_directory(&self, generation: SearchGenerationId) -> PathBuf {
        self.generations.join(generation.directory_name())
    }

    pub fn begin(&mut self) -> Result<GenerationBuild, GenerationStoreError> {
        let claim = self.live_build.acquire()?;
        loop {
            let ordinal = self.next_staging_ordinal;
            self.next_staging_ordinal = ordinal
                .checked_add(1)
                .ok_or(GenerationStoreError::OrdinalOverflow)?;
            let directory = self.staging.join(staging_directory_name(ordinal));

            match fs::create_dir(&directory) {
                Ok(()) => {
                    let build = GenerationBuild {
                        store_root: self.root.clone(),
                        ordinal,
                        directory,
                        lease: Arc::clone(&self.lease),
                        claim,
                        state: GenerationBuildState::Armed,
                    };
                    create_build_directories(&build.directory)?;
                    return Ok(build);
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(GenerationStoreError::io(
                        "create generation staging directory",
                        directory,
                        source,
                    ));
                }
            }
        }
    }

    #[cfg(test)]
    pub fn measure_artifacts(
        &self,
        build: &GenerationBuild,
    ) -> Result<GenerationArtifactEvidence, GenerationStoreError> {
        let mut budget = AssetLoadBudget::default();
        self.measure_artifacts_with_budget(build, &mut budget)
    }

    pub fn measure_artifacts_with_budget(
        &self,
        build: &GenerationBuild,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationArtifactEvidence, GenerationStoreError> {
        self.validate_build(build)?;
        measure_generation_artifacts(&build.directory, budget, None)
    }

    #[cfg(test)]
    pub fn prepare_publish(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        let mut budget = AssetLoadBudget::default();
        let desired_revision = manifest.revision();
        self.prepare_publish_inner(build, manifest, desired_revision, &mut budget, None)
    }

    pub(crate) fn prepare_publish_with_desired_revision_and_budget(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        desired_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        self.prepare_publish_inner(build, manifest, desired_revision, budget, None)
    }

    #[cfg(test)]
    pub fn prepare_publish_with_failpoint(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        failpoint: GenerationFailpoint,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        let mut budget = AssetLoadBudget::default();
        self.prepare_publish_with_failpoint_and_budget(build, manifest, &mut budget, failpoint)
    }

    #[cfg(test)]
    pub fn prepare_publish_with_failpoint_and_budget(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        budget: &mut AssetLoadBudget,
        failpoint: GenerationFailpoint,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        let desired_revision = manifest.revision();
        self.prepare_publish_inner(build, manifest, desired_revision, budget, Some(failpoint))
    }

    #[cfg(test)]
    pub(crate) fn prepare_publish_with_desired_revision_failpoint_and_budget(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        desired_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
        failpoint: GenerationFailpoint,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        self.prepare_publish_inner(build, manifest, desired_revision, budget, Some(failpoint))
    }

    pub fn estimate_publish(
        &self,
        new_generation_bytes: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationDiskEstimate, GenerationStoreError> {
        let existing_generations = completed_generation_sizes(&self.generations, budget)?;
        let existing_generation_bytes = checked_sum(
            existing_generations.iter().map(|(_, bytes)| *bytes),
            "existing generation bytes",
        )?;
        let old_active_generation_bytes = self
            .active
            .as_ref()
            .and_then(|active| {
                existing_generations
                    .iter()
                    .find(|(generation, _)| *generation == active.generation)
                    .map(|(_, bytes)| *bytes)
            })
            .unwrap_or(0);

        let historical = self.retained_historical_snapshots(budget)?;
        let mut retained_after_publish = Vec::new();
        if self.options.retain_previous_generations != 0 {
            if let Some(active) = &self.active {
                retained_after_publish.push(active.generation);
            }
            retained_after_publish.extend(
                historical
                    .into_iter()
                    .map(|generation| generation.generation)
                    .take(self.options.retain_previous_generations.saturating_sub(1)),
            );
        }
        let retained_historical_bytes = checked_sum(
            retained_after_publish.iter().filter_map(|generation| {
                existing_generations
                    .iter()
                    .find(|(candidate, _)| candidate == generation)
                    .map(|(_, bytes)| *bytes)
            }),
            "retained historical generation bytes",
        )?;
        let retained_bytes_after_publish = new_generation_bytes
            .checked_add(retained_historical_bytes)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "retained bytes after publish",
            })?;
        let publish_peak_bytes = existing_generation_bytes
            .checked_add(new_generation_bytes)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "publish peak bytes",
            })?;
        let reclaimable_bytes_after_publish =
            publish_peak_bytes.saturating_sub(retained_bytes_after_publish);

        Ok(GenerationDiskEstimate {
            existing_generation_bytes,
            old_active_generation_bytes,
            new_generation_bytes,
            publish_peak_bytes,
            retained_bytes_after_publish,
            reclaimable_bytes_after_publish,
        })
    }

    pub fn estimate_manifest_publish(
        &self,
        manifest: &SearchGenerationManifestV1,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationDiskEstimate, GenerationStoreError> {
        let manifest_path = self.staging.join(MANIFEST_FILE);
        let manifest_bytes = generation_manifest_json_length(manifest, &manifest_path)?;
        budget
            .check_bytes(manifest_bytes)
            .and_then(|()| budget.consume_bytes(manifest_bytes))
            .map_err(GenerationStoreError::Budget)?;
        let artifact_bytes =
            manifest
                .artifacts()
                .total_bytes()
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "incoming generation artifact bytes",
                })?;
        let new_generation_bytes = artifact_bytes.checked_add(manifest_bytes).ok_or(
            GenerationStoreError::SizeOverflow {
                resource: "incoming generation bytes",
            },
        )?;
        self.estimate_publish(new_generation_bytes, budget)
    }

    fn prepare_publish_inner(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        desired_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
        failpoint: Option<GenerationFailpoint>,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        self.validate_build(build)?;

        let observed = measure_generation_artifacts(&build.directory, budget, failpoint)?;
        if observed != manifest.artifacts() {
            return Err(GenerationStoreError::ArtifactEvidenceMismatch {
                expected: Box::new(manifest.artifacts()),
                actual: Box::new(observed),
            });
        }
        validate_persisted_source_state(&build.directory, &manifest)?;

        let generation = manifest.generation_id();
        if let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation == generation)
            .cloned()
        {
            let completed = inspect_completed_generation(&active.directory, generation, budget)?;
            build.abort_with_budget(budget)?;
            if active.desired_revision != desired_revision {
                let mut refreshed = active;
                refreshed.activation_ordinal = self.allocate_activation_ordinal()?;
                refreshed.manifest_digest = completed.manifest_digest;
                refreshed.desired_revision = desired_revision;
                return Ok(PreparedGenerationPublish {
                    store: self,
                    snapshot: refreshed,
                    activation: PreparedActivation::Activate {
                        manifest_digest: completed.manifest_digest,
                    },
                    warnings: platform_durability_warnings(),
                    failpoint,
                });
            }
            return Ok(PreparedGenerationPublish {
                store: self,
                snapshot: active,
                activation: PreparedActivation::AlreadyActive,
                warnings: Vec::new(),
                failpoint,
            });
        }
        self.validate_manifest_parent(&manifest)?;

        let completed_directory = self.generation_directory(generation);
        let replace_invalid_completed = if path_exists_no_follow(&completed_directory)? {
            match inspect_completed_generation(&completed_directory, generation, budget) {
                Ok(completed) => {
                    build.abort_with_budget(budget)?;
                    let activation_ordinal = self.allocate_activation_ordinal()?;
                    let snapshot = GenerationSnapshot {
                        activation_ordinal,
                        generation,
                        manifest_digest: completed.manifest_digest,
                        desired_revision,
                        manifest: completed.manifest,
                        directory: completed_directory,
                    };
                    return Ok(PreparedGenerationPublish {
                        store: self,
                        snapshot,
                        activation: PreparedActivation::Activate {
                            manifest_digest: completed.manifest_digest,
                        },
                        warnings: platform_durability_warnings(),
                        failpoint,
                    });
                }
                Err(error) if error.is_repairable_completed_generation() => true,
                Err(error) => return Err(error),
            }
        } else {
            false
        };

        let manifest_path = build.directory.join(MANIFEST_FILE);
        let manifest_bytes = encode_generation_manifest_json(&manifest, &manifest_path, budget)?;
        write_new_file(&manifest_path, &manifest_bytes)?;
        sync_tree_no_follow(&build.directory, budget)?;
        let durable_observed = measure_generation_artifacts(&build.directory, budget, None)?;
        if durable_observed != manifest.artifacts() {
            return Err(GenerationStoreError::ArtifactEvidenceMismatch {
                expected: Box::new(manifest.artifacts()),
                actual: Box::new(durable_observed),
            });
        }

        let quarantine = if replace_invalid_completed {
            let quarantine = self
                .staging
                .join(quarantine_directory_name(build.ordinal, generation));
            if path_exists_no_follow(&quarantine)? {
                return Err(GenerationStoreError::QuarantineCollision { path: quarantine });
            }
            fs::rename(&completed_directory, &quarantine).map_err(|source| {
                GenerationStoreError::io(
                    "quarantine invalid completed generation",
                    completed_directory.clone(),
                    source,
                )
            })?;
            if let Err(primary) =
                sync_directory(&self.generations).and_then(|()| sync_directory(&self.staging))
            {
                return Err(self.rollback_quarantine(&quarantine, &completed_directory, primary));
            }
            Some(quarantine)
        } else {
            None
        };

        if let Err(source) = fs::rename(&build.directory, &completed_directory) {
            let primary = GenerationStoreError::io(
                "complete generation directory",
                completed_directory.clone(),
                source,
            );
            if let Some(quarantine) = &quarantine {
                return Err(self.rollback_quarantine(quarantine, &completed_directory, primary));
            }
            return Err(primary);
        }
        build.mark_completed();
        sync_directory(&self.generations)?;

        let mut warnings = Vec::new();
        if let Some(quarantine) = quarantine {
            if let Err(error) = remove_tree_no_follow(&quarantine, budget) {
                warnings.push(GenerationPublishWarning::new(
                    GenerationPublishWarningKind::PreparationCleanup,
                    error.to_string(),
                ));
            } else if let Err(error) = sync_directory(&self.staging) {
                warnings.push(GenerationPublishWarning::new(
                    GenerationPublishWarningKind::PreparationCleanup,
                    error.to_string(),
                ));
            }
        }
        let manifest_digest = DigestV1::hash_bytes(&manifest_bytes);
        warnings.extend(platform_durability_warnings());
        let activation_ordinal = self.allocate_activation_ordinal()?;
        let snapshot = GenerationSnapshot {
            activation_ordinal,
            generation,
            manifest_digest,
            desired_revision,
            manifest,
            directory: completed_directory,
        };
        Ok(PreparedGenerationPublish {
            store: self,
            snapshot,
            activation: PreparedActivation::Activate { manifest_digest },
            warnings,
            failpoint,
        })
    }

    fn rollback_quarantine(
        &self,
        quarantine: &Path,
        completed_directory: &Path,
        primary: GenerationStoreError,
    ) -> GenerationStoreError {
        let rollback = fs::rename(quarantine, completed_directory)
            .map_err(|source| {
                GenerationStoreError::io(
                    "restore quarantined generation",
                    completed_directory.to_path_buf(),
                    source,
                )
            })
            .and_then(|()| sync_directory(&self.generations))
            .and_then(|()| sync_directory(&self.staging));
        match rollback {
            Ok(()) => primary,
            Err(rollback) => GenerationStoreError::QuarantineRollbackFailed {
                primary: Box::new(primary),
                rollback: Box::new(rollback),
            },
        }
    }

    fn activate_prepared(
        &mut self,
        snapshot: GenerationSnapshot,
        manifest_digest: DigestV1,
        mut warnings: Vec<GenerationPublishWarning>,
        failpoint: Option<GenerationFailpoint>,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublishReport, GenerationStoreError> {
        let record = GenerationHeadRecord {
            contract_version: GENERATION_HEAD_CONTRACT_VERSION,
            ordinal: snapshot.activation_ordinal,
            generation: snapshot.generation,
            manifest_digest,
            workspace: snapshot.manifest.workspace(),
            revision: snapshot.manifest.revision(),
            desired_revision: Some(snapshot.desired_revision),
        };
        warnings.extend(self.write_activation(&record, failpoint, budget)?);

        self.active = Some(snapshot.clone());

        // Retention is post-commit maintenance. A security violation cannot turn this committed
        // activation into a failed publication; reopening rescans managed directories without
        // following links and fails closed if the unsafe entry remains.
        let mut maintenance_budget = AssetLoadBudget::default();
        let pruned_generations = match self.prune_retention(&mut maintenance_budget) {
            Ok(pruned) => pruned,
            Err(error) => {
                warnings.push(GenerationPublishWarning::new(
                    GenerationPublishWarningKind::Retention,
                    error.to_string(),
                ));
                Vec::new()
            }
        };
        Ok(GenerationPublishReport {
            active: snapshot,
            pruned_generations,
            warnings,
        })
    }

    fn validate_build(&self, build: &GenerationBuild) -> Result<(), GenerationStoreError> {
        if build.store_root != self.root
            || !Arc::ptr_eq(&build.lease, &self.lease)
            || !build.claim.belongs_to(&self.live_build)
            || build.state != GenerationBuildState::Armed
            || build.directory.parent() != Some(self.staging.as_path())
            || build.directory.file_name()
                != Some(OsStr::new(&staging_directory_name(build.ordinal)))
        {
            return Err(GenerationStoreError::ForeignBuild);
        }
        ensure_existing_directory_no_follow(&build.directory)?;
        Ok(())
    }

    fn validate_manifest_parent(
        &self,
        manifest: &SearchGenerationManifestV1,
    ) -> Result<(), GenerationStoreError> {
        if let Some(active) = &self.active
            && active.manifest.workspace() != manifest.workspace()
        {
            return Err(GenerationStoreError::WorkspaceMismatch {
                expected: active.manifest.workspace(),
                actual: manifest.workspace(),
            });
        }
        if let Some(expected_parent) = manifest.parent_generation() {
            let actual_parent = self.active.as_ref().map(GenerationSnapshot::generation);
            if actual_parent != Some(expected_parent) {
                return Err(GenerationStoreError::ParentGenerationMismatch {
                    expected: expected_parent,
                    actual: actual_parent,
                });
            }
        }
        Ok(())
    }

    fn allocate_activation_ordinal(&mut self) -> Result<u64, GenerationStoreError> {
        loop {
            let ordinal = self.next_activation_ordinal;
            self.next_activation_ordinal = ordinal
                .checked_add(1)
                .ok_or(GenerationStoreError::OrdinalOverflow)?;
            let final_path = self.activations.join(activation_file_name(ordinal));
            let temporary_path = self.staging.join(activation_staging_file_name(ordinal));
            if !path_exists_no_follow(&final_path)? && !path_exists_no_follow(&temporary_path)? {
                return Ok(ordinal);
            }
        }
    }

    fn write_activation(
        &self,
        record: &GenerationHeadRecord,
        failpoint: Option<GenerationFailpoint>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<GenerationPublishWarning>, GenerationStoreError> {
        let temporary_path = self
            .staging
            .join(activation_staging_file_name(record.ordinal));
        let final_path = self.activations.join(activation_file_name(record.ordinal));
        let bytes = encode_generation_head_json(record, &final_path, budget)?;
        let mut staging = ActivationStagingFile::create(temporary_path, &bytes)?;
        // A hard link publishes the complete file atomically and never replaces an existing ordinal.
        staging.publish(&final_path, failpoint)?;

        // The final hard link is the commit point: reopen selects this highest valid activation
        // immediately after it becomes visible. Every later failure is reported as evidence rather
        // than returning an error that would disagree with the persisted active generation.
        let mut warnings = Vec::new();
        if let Err(error) = inject_failure(failpoint, GenerationFailpoint::ActivationDirectorySync)
            .and_then(|()| sync_directory(&self.activations))
        {
            warnings.push(GenerationPublishWarning::new(
                GenerationPublishWarningKind::PostCommitDurability,
                error.to_string(),
            ));
        }

        if let Err(error) = inject_failure(failpoint, GenerationFailpoint::ActivationCleanup)
            .and_then(|()| staging.cleanup_after_commit())
        {
            staging.state = ActivationStagingState::Relinquished;
            warnings.push(GenerationPublishWarning::new(
                GenerationPublishWarningKind::PostCommitCleanup,
                error.to_string(),
            ));
        }
        Ok(warnings)
    }

    fn retained_historical_snapshots(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<GenerationSnapshot>, GenerationStoreError> {
        let Some(active) = &self.active else {
            return Ok(Vec::new());
        };
        let opened_activations =
            SecureReadDirectory::open(&self.activations).map_err(|source| {
                persisted_read_error(
                    "open activations directory",
                    self.activations.clone(),
                    source,
                )
            })?;
        let opened_generations =
            SecureReadDirectory::open(&self.generations).map_err(|source| {
                persisted_read_error(
                    "open generations directory",
                    self.generations.clone(),
                    source,
                )
            })?;
        let (mut candidates, _) =
            activation_candidates_for_open(&self.activations, &self.staging, budget)?;
        candidates.sort_unstable_by_key(|candidate| std::cmp::Reverse(candidate.ordinal));

        let mut seen = BTreeSet::new();
        seen.insert(active.generation);
        let mut retained = Vec::new();
        for candidate in candidates {
            if retained.len() >= self.options.retain_previous_generations {
                break;
            }
            let record = match read_activation_record(
                &opened_activations,
                &candidate.path,
                &candidate.file_name,
                candidate.ordinal,
                budget,
            ) {
                Ok(record) => record,
                Err(error) if error.is_candidate_scan_fatal() => {
                    return Err(error);
                }
                Err(_) => continue,
            };
            if seen.contains(&record.generation) {
                continue;
            }
            let generation = match load_completed_generation(
                &self.generations,
                &opened_generations,
                &record,
                budget,
            ) {
                Ok(generation) => generation,
                Err(error) if error.is_candidate_scan_fatal() => {
                    return Err(error);
                }
                Err(_) => continue,
            };
            if seen.insert(generation.generation) {
                retained.push(generation);
            }
        }
        Ok(retained)
    }

    fn prune_retention(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SearchGenerationId>, GenerationStoreError> {
        let Some(active) = &self.active else {
            return Ok(Vec::new());
        };
        let historical = self.retained_historical_snapshots(budget)?;
        let retained_directories = historical
            .iter()
            .map(GenerationSnapshot::generation)
            .chain(std::iter::once(active.generation))
            .map(SearchGenerationId::directory_name)
            .collect::<BTreeSet<_>>();
        let retained_activations = historical
            .iter()
            .map(GenerationSnapshot::activation_ordinal)
            .chain(std::iter::once(active.activation_ordinal))
            .collect::<BTreeSet<_>>();

        let mut pruned = Vec::new();
        visit_directory_entries_budgeted(&self.generations, budget, |entry, budget| {
            let metadata = metadata_no_follow(&entry.path)?;
            if !metadata.is_dir() {
                return Ok(());
            }
            let Some(name) = entry.file_name.to_str() else {
                return Ok(());
            };
            let Some(generation) = SearchGenerationId::from_directory_name(name) else {
                return Ok(());
            };
            if retained_directories.contains(name) {
                return Ok(());
            }
            remove_tree_no_follow(&entry.path, budget)?;
            reserve_artifact_vec(&mut pruned, 1, "pruned generation IDs", budget)?;
            pruned.push(generation);
            Ok(())
        })?;
        if !pruned.is_empty() {
            sync_directory(&self.generations)?;
        }
        prune_activation_history(&self.activations, &retained_activations, budget)?;
        Ok(pruned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationHeadRecord {
    contract_version: u16,
    ordinal: u64,
    generation: SearchGenerationId,
    manifest_digest: DigestV1,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    desired_revision: Option<WorkspaceRevision>,
}

impl GenerationHeadRecord {
    fn desired_revision(&self) -> WorkspaceRevision {
        self.desired_revision.unwrap_or(self.revision)
    }
}

#[derive(Debug)]
struct ActivationCandidate {
    ordinal: u64,
    path: PathBuf,
    file_name: OsString,
}

#[derive(Debug)]
struct CompletedGeneration {
    manifest: SearchGenerationManifestV1,
    manifest_digest: DigestV1,
}

fn initialize_root(root: &Path) -> Result<PathBuf, GenerationStoreError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            reject_link_or_reparse(root, &metadata)?;
            if !metadata.is_dir() {
                return Err(GenerationStoreError::NotDirectory {
                    path: root.to_path_buf(),
                });
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|source| {
                GenerationStoreError::io("create generation store root", root.to_path_buf(), source)
            })?;
        }
        Err(source) => {
            return Err(GenerationStoreError::io(
                "inspect generation store root",
                root.to_path_buf(),
                source,
            ));
        }
    }
    fs::canonicalize(root).map_err(|source| {
        GenerationStoreError::io(
            "canonicalize generation store root",
            root.to_path_buf(),
            source,
        )
    })
}

fn ensure_managed_directory(
    root: &Path,
    name: &'static str,
) -> Result<PathBuf, GenerationStoreError> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            reject_link_or_reparse(&path, &metadata)?;
            if !metadata.is_dir() {
                return Err(GenerationStoreError::NotDirectory { path });
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&path).map_err(|source| {
                GenerationStoreError::io(
                    "create managed generation directory",
                    path.clone(),
                    source,
                )
            })?;
        }
        Err(source) => {
            return Err(GenerationStoreError::io(
                "inspect managed generation directory",
                path,
                source,
            ));
        }
    }
    Ok(path)
}

fn ensure_existing_directory_no_follow(path: &Path) -> Result<(), GenerationStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        GenerationStoreError::io("inspect generation directory", path.to_path_buf(), source)
    })?;
    reject_link_or_reparse(path, &metadata)?;
    if !metadata.is_dir() {
        return Err(GenerationStoreError::NotDirectory {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn create_build_directories(directory: &Path) -> Result<(), GenerationStoreError> {
    for name in [
        SEARCH_ARTIFACT_DIRECTORY,
        REFERENCE_ARTIFACT_DIRECTORY,
        SOURCE_STATE_ARTIFACT_DIRECTORY,
    ] {
        let path = directory.join(name);
        fs::create_dir(&path).map_err(|source| {
            GenerationStoreError::io("create generation artifact directory", path, source)
        })?;
    }
    Ok(())
}

struct BudgetedDirectoryEntry {
    path: PathBuf,
    file_name: OsString,
}

fn visit_directory_entries_budgeted(
    directory: &Path,
    budget: &mut AssetLoadBudget,
    mut visitor: impl FnMut(
        BudgetedDirectoryEntry,
        &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError>,
) -> Result<(), GenerationStoreError> {
    let entries = fs::read_dir(directory).map_err(|source| {
        GenerationStoreError::io("read generation directory", directory.to_path_buf(), source)
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            GenerationStoreError::io(
                "read generation directory entry",
                directory.to_path_buf(),
                source,
            )
        })?;
        let path = entry.path();
        let file_name = entry.file_name();
        let fixed_bytes = size_of::<fs::DirEntry>()
            .checked_add(size_of::<PathBuf>())
            .and_then(|bytes| bytes.checked_add(size_of::<OsString>()))
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "generation directory entry",
            })?;
        let accounted_bytes = u64::try_from(fixed_bytes)
            .ok()
            .and_then(|bytes| bytes.checked_add(u64::try_from(path.as_os_str().len()).ok()?))
            .and_then(|bytes| bytes.checked_add(u64::try_from(file_name.len()).ok()?))
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "generation directory entry",
            })?;
        budget
            .check_entries(1)
            .and_then(|()| budget.check_bytes(accounted_bytes))
            .map_err(GenerationStoreError::Budget)?;
        budget
            .consume_entries(1)
            .and_then(|()| budget.consume_bytes(accounted_bytes))
            .map_err(GenerationStoreError::Budget)?;
        visitor(BudgetedDirectoryEntry { path, file_name }, budget)?;
    }
    Ok(())
}

fn recover_owned_staging(
    staging: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<GenerationStagingRecoveryReport, GenerationStoreError> {
    let mut changed = false;
    let mut removed_entries = 0_u64;
    visit_directory_entries_budgeted(staging, budget, |entry, budget| {
        let metadata = metadata_no_follow(&entry.path)?;
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        if parse_staging_directory_name(name).is_some() {
            if !metadata.is_dir() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            remove_tree_no_follow(&entry.path, budget)?;
            changed = true;
            removed_entries =
                removed_entries
                    .checked_add(1)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "recovered staging entries",
                    })?;
            return Ok(());
        }
        if parse_quarantine_directory_name(name).is_some() {
            if !metadata.is_dir() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            remove_tree_no_follow(&entry.path, budget)?;
            changed = true;
            removed_entries =
                removed_entries
                    .checked_add(1)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "recovered staging entries",
                    })?;
            return Ok(());
        }
        if parse_activation_staging_file_name(name).is_some() {
            if !metadata.is_file() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            fs::remove_file(&entry.path).map_err(|source| {
                GenerationStoreError::io(
                    "remove abandoned activation staging file",
                    entry.path,
                    source,
                )
            })?;
            changed = true;
            removed_entries =
                removed_entries
                    .checked_add(1)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "recovered staging entries",
                    })?;
        }
        Ok(())
    })?;
    if changed {
        sync_directory(staging)?;
    }
    Ok(GenerationStagingRecoveryReport { removed_entries })
}

fn next_staging_ordinal(
    staging: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<u64, GenerationStoreError> {
    let mut maximum = 0_u64;
    visit_directory_entries_budgeted(staging, budget, |entry, _budget| {
        let _ = metadata_no_follow(&entry.path)?;
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        if let Some(ordinal) = parse_staging_directory_name(name) {
            maximum = maximum.max(ordinal);
        }
        Ok(())
    })?;
    maximum
        .checked_add(1)
        .ok_or(GenerationStoreError::OrdinalOverflow)
}

fn activation_candidates_for_open(
    activations: &Path,
    staging: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(Vec<ActivationCandidate>, u64), GenerationStoreError> {
    let mut candidates = Vec::new();
    let mut maximum = 0_u64;
    visit_directory_entries_budgeted(activations, budget, |entry, budget| {
        let _ = metadata_no_follow(&entry.path)?;
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        let Some(ordinal) = parse_activation_file_name(name) else {
            return Ok(());
        };
        maximum = maximum.max(ordinal);
        push_activation_candidate(
            &mut candidates,
            ActivationCandidate {
                ordinal,
                path: entry.path,
                file_name: entry.file_name,
            },
            Some(budget),
        )
    })?;
    visit_directory_entries_budgeted(staging, budget, |entry, _budget| {
        let _ = metadata_no_follow(&entry.path)?;
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        if let Some(ordinal) = parse_activation_staging_file_name(name) {
            maximum = maximum.max(ordinal);
        }
        Ok(())
    })?;
    candidates.sort_unstable_by_key(|candidate| candidate.ordinal);
    let next = maximum
        .checked_add(1)
        .ok_or(GenerationStoreError::OrdinalOverflow)?;
    Ok((candidates, next))
}

fn push_activation_candidate(
    candidates: &mut Vec<ActivationCandidate>,
    candidate: ActivationCandidate,
    mut budget: Option<&mut AssetLoadBudget>,
) -> Result<(), GenerationStoreError> {
    if candidates.len() >= MAX_ACTIVATION_CANDIDATES {
        return Err(GenerationStoreError::ActivationCandidateLimitExceeded {
            maximum: MAX_ACTIVATION_CANDIDATES,
        });
    }
    if candidates.len() == candidates.capacity() {
        let additional = ACTIVATION_CANDIDATE_GROWTH.min(
            MAX_ACTIVATION_CANDIDATES
                .checked_sub(candidates.len())
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "activation candidate capacity",
                })?,
        );
        let requested_bytes = additional
            .checked_mul(size_of::<ActivationCandidate>())
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "activation candidate vector",
            })?;
        if let Some(budget) = budget.as_deref_mut() {
            budget
                .check_bytes(u64::try_from(requested_bytes).map_err(|_| {
                    GenerationStoreError::SizeOverflow {
                        resource: "activation candidate vector",
                    }
                })?)
                .map_err(GenerationStoreError::Budget)?;
        }
        let previous_capacity = candidates.capacity();
        candidates.try_reserve_exact(additional).map_err(|_| {
            GenerationStoreError::AllocationFailed {
                resource: "activation candidate vector",
                requested: requested_bytes,
            }
        })?;
        if let Some(budget) = budget {
            let added_capacity = candidates.capacity().checked_sub(previous_capacity).ok_or(
                GenerationStoreError::SizeOverflow {
                    resource: "activation candidate vector",
                },
            )?;
            let allocated_bytes = added_capacity
                .checked_mul(size_of::<ActivationCandidate>())
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "activation candidate vector",
                })?;
            budget
                .consume_bytes(allocated_bytes)
                .map_err(GenerationStoreError::Budget)?;
        }
    }
    candidates.push(candidate);
    Ok(())
}

fn prune_activation_history(
    activations: &Path,
    retained_ordinals: &BTreeSet<u64>,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let mut changed = false;
    visit_directory_entries_budgeted(activations, budget, |entry, _budget| {
        let path = entry.path;
        let metadata = metadata_no_follow(&path)?;
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        let Some(ordinal) = parse_activation_file_name(name) else {
            return Ok(());
        };
        if retained_ordinals.contains(&ordinal) {
            return Ok(());
        }
        if !metadata.is_file() {
            return Err(GenerationStoreError::UnsupportedFileType { path });
        }
        fs::remove_file(&path).map_err(|source| {
            GenerationStoreError::io("prune generation activation record", path, source)
        })?;
        changed = true;
        Ok(())
    })?;
    if changed {
        sync_directory(activations)?;
    }
    Ok(())
}

fn select_active_generation(
    generations: &Path,
    opened_generations: &SecureReadDirectory,
    opened_activations: &SecureReadDirectory,
    candidates: &[ActivationCandidate],
    budget: &mut AssetLoadBudget,
) -> Result<Option<GenerationSnapshot>, GenerationStoreError> {
    let Some(candidate) = candidates.last() else {
        return Ok(None);
    };
    let record = read_activation_record(
        opened_activations,
        &candidate.path,
        &candidate.file_name,
        candidate.ordinal,
        budget,
    )?;
    load_completed_generation(generations, opened_generations, &record, budget).map(Some)
}

fn read_activation_record(
    directory: &SecureReadDirectory,
    path: &Path,
    file_name: &OsStr,
    expected_ordinal: u64,
    budget: &mut AssetLoadBudget,
) -> Result<GenerationHeadRecord, GenerationStoreError> {
    let mut file = open_contract_file(
        directory,
        file_name,
        path,
        MAX_ACTIVATION_BYTES_U64,
        "activation record",
    )?;
    let decoded =
        read_contract_json::<GenerationHeadRecord>(file.file_mut(), budget, ACTIVATION_JSON_LIMITS);
    file.ensure_unchanged().map_err(|source| {
        persisted_read_error("revalidate activation record", path.to_path_buf(), source)
    })?;
    let record = decoded.map_err(|source| GenerationStoreError::ContractJson {
        artifact: "activation record",
        path: path.to_path_buf(),
        source,
    })?;
    match (record.contract_version, record.desired_revision) {
        (LEGACY_ACTIVATION_CONTRACT_VERSION, None)
        | (GENERATION_HEAD_CONTRACT_VERSION, Some(_)) => {}
        (LEGACY_ACTIVATION_CONTRACT_VERSION, Some(_)) => {
            return Err(GenerationStoreError::InvalidGenerationHead {
                path: path.to_path_buf(),
                message: "legacy activation record contains a desired revision",
            });
        }
        (GENERATION_HEAD_CONTRACT_VERSION, None) => {
            return Err(GenerationStoreError::InvalidGenerationHead {
                path: path.to_path_buf(),
                message: "generation head is missing its desired revision",
            });
        }
        (actual, _) => {
            return Err(GenerationStoreError::UnsupportedVersion {
                artifact: "generation head",
                actual,
                expected: GENERATION_HEAD_CONTRACT_VERSION,
            });
        }
    }
    if record.ordinal != expected_ordinal {
        return Err(GenerationStoreError::ActivationOrdinalMismatch {
            path: path.to_path_buf(),
            expected: expected_ordinal,
            actual: record.ordinal,
        });
    }
    Ok(record)
}

fn load_completed_generation(
    generations: &Path,
    opened_generations: &SecureReadDirectory,
    record: &GenerationHeadRecord,
    budget: &mut AssetLoadBudget,
) -> Result<GenerationSnapshot, GenerationStoreError> {
    let directory_name = record.generation.directory_name();
    let directory = generations.join(&directory_name);
    let opened_directory = opened_generations
        .open_directory(OsStr::new(&directory_name))
        .map_err(|source| {
            persisted_read_error(
                "open completed generation directory",
                directory.clone(),
                source,
            )
        })?;
    let completed =
        inspect_completed_generation_in(&directory, &opened_directory, record.generation, budget)?;
    if completed.manifest_digest != record.manifest_digest {
        return Err(GenerationStoreError::ManifestDigestMismatch {
            generation: record.generation,
            expected: record.manifest_digest,
            actual: completed.manifest_digest,
        });
    }
    if completed.manifest.workspace() != record.workspace
        || completed.manifest.revision() != record.revision
    {
        return Err(GenerationStoreError::ActivationContextMismatch {
            generation: record.generation,
        });
    }
    Ok(GenerationSnapshot {
        activation_ordinal: record.ordinal,
        generation: record.generation,
        manifest_digest: completed.manifest_digest,
        desired_revision: record.desired_revision(),
        manifest: completed.manifest,
        directory,
    })
}

fn inspect_completed_generation(
    directory: &Path,
    expected_generation: SearchGenerationId,
    budget: &mut AssetLoadBudget,
) -> Result<CompletedGeneration, GenerationStoreError> {
    let opened_directory = SecureReadDirectory::open(directory).map_err(|source| {
        persisted_read_error(
            "open completed generation directory",
            directory.to_path_buf(),
            source,
        )
    })?;
    inspect_completed_generation_in(directory, &opened_directory, expected_generation, budget)
}

fn inspect_completed_generation_in(
    directory: &Path,
    opened_directory: &SecureReadDirectory,
    expected_generation: SearchGenerationId,
    budget: &mut AssetLoadBudget,
) -> Result<CompletedGeneration, GenerationStoreError> {
    let manifest_path = directory.join(MANIFEST_FILE);
    let mut manifest_file = open_contract_file(
        opened_directory,
        OsStr::new(MANIFEST_FILE),
        &manifest_path,
        MAX_MANIFEST_BYTES_U64,
        "generation manifest",
    )?;
    let manifest_length = manifest_file.length();
    let mut reader = DigestingReader::new(manifest_file.file_mut(), manifest_length);
    let decoded =
        read_contract_json::<SearchGenerationManifestV1>(&mut reader, budget, MANIFEST_JSON_LIMITS);
    let manifest_digest = reader.finalize();
    manifest_file.ensure_unchanged().map_err(|source| {
        persisted_read_error(
            "revalidate generation manifest",
            manifest_path.clone(),
            source,
        )
    })?;
    let manifest = decoded.map_err(|source| GenerationStoreError::ContractJson {
        artifact: "generation manifest",
        path: manifest_path.clone(),
        source,
    })?;
    let manifest_digest = manifest_digest.map_err(|source| {
        GenerationStoreError::io(
            "digest generation manifest",
            manifest_path,
            io::Error::other(source),
        )
    })?;
    let actual_generation = manifest.generation_id();
    if actual_generation != expected_generation {
        return Err(GenerationStoreError::ManifestGenerationMismatch {
            expected: expected_generation,
            actual: actual_generation,
        });
    }
    let actual_artifacts =
        measure_generation_artifacts_in(directory, opened_directory, budget, None)?;
    if actual_artifacts != manifest.artifacts() {
        return Err(GenerationStoreError::ArtifactEvidenceMismatch {
            expected: Box::new(manifest.artifacts()),
            actual: Box::new(actual_artifacts),
        });
    }
    let source_state_directory = directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY);
    let opened_source_state = opened_directory
        .open_directory(OsStr::new(SOURCE_STATE_ARTIFACT_DIRECTORY))
        .map_err(|source| {
            persisted_read_error(
                "open completed source-state directory",
                source_state_directory.clone(),
                source,
            )
        })?;
    validate_persisted_source_state_in(
        directory,
        &source_state_directory,
        &opened_source_state,
        &manifest,
    )?;
    Ok(CompletedGeneration {
        manifest,
        manifest_digest,
    })
}

fn measure_generation_artifacts(
    directory: &Path,
    budget: &mut AssetLoadBudget,
    failpoint: Option<GenerationFailpoint>,
) -> Result<GenerationArtifactEvidence, GenerationStoreError> {
    let opened = SecureReadDirectory::open(directory).map_err(|source| {
        persisted_read_error(
            "open generation artifact root",
            directory.to_path_buf(),
            source,
        )
    })?;
    measure_generation_artifacts_in(directory, &opened, budget, failpoint)
}

fn measure_generation_artifacts_in(
    directory: &Path,
    opened_directory: &SecureReadDirectory,
    budget: &mut AssetLoadBudget,
    failpoint: Option<GenerationFailpoint>,
) -> Result<GenerationArtifactEvidence, GenerationStoreError> {
    inject_failure(failpoint, GenerationFailpoint::Search)?;
    let search_directory = directory.join(SEARCH_ARTIFACT_DIRECTORY);
    let opened_search = opened_directory
        .open_directory(OsStr::new(SEARCH_ARTIFACT_DIRECTORY))
        .map_err(|source| {
            persisted_read_error(
                "open search artifact directory",
                search_directory.clone(),
                source,
            )
        })?;
    let search = measure_anchored_artifact_tree(
        &directory.join(SEARCH_ARTIFACT_DIRECTORY),
        opened_search,
        budget,
    )?;
    inject_failure(failpoint, GenerationFailpoint::References)?;
    let reference_directory = directory.join(REFERENCE_ARTIFACT_DIRECTORY);
    let opened_references = opened_directory
        .open_directory(OsStr::new(REFERENCE_ARTIFACT_DIRECTORY))
        .map_err(|source| {
            persisted_read_error(
                "open reference artifact directory",
                reference_directory.clone(),
                source,
            )
        })?;
    let references = measure_anchored_artifact_tree(
        &directory.join(REFERENCE_ARTIFACT_DIRECTORY),
        opened_references,
        budget,
    )?;
    inject_failure(failpoint, GenerationFailpoint::SourceState)?;
    let source_state_directory = directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY);
    let opened_source_state = opened_directory
        .open_directory(OsStr::new(SOURCE_STATE_ARTIFACT_DIRECTORY))
        .map_err(|source| {
            persisted_read_error(
                "open source-state artifact directory",
                source_state_directory.clone(),
                source,
            )
        })?;
    let source_state = measure_anchored_artifact_tree(
        &directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY),
        opened_source_state,
        budget,
    )?;
    Ok(GenerationArtifactEvidence::new(
        search,
        references,
        source_state,
    ))
}

pub(crate) fn measure_artifact_tree(
    root: &Path,
) -> Result<ArtifactTreeEvidence, GenerationStoreError> {
    let mut budget = AssetLoadBudget::default();
    measure_artifact_tree_with_budget(root, &mut budget)
}

fn measure_artifact_tree_with_budget(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<ArtifactTreeEvidence, GenerationStoreError> {
    ensure_existing_directory_no_follow(root)?;
    let mut pending = Vec::new();
    reserve_artifact_vec(
        &mut pending,
        1,
        "artifact directory traversal stack",
        budget,
    )?;
    pending.push((root.to_path_buf(), 0_u32));
    let mut entries = Vec::new();
    let mut total_bytes = 0_u64;
    let mut directory_count = 0_u64;

    while let Some((directory, depth)) = pending.pop() {
        directory_count =
            directory_count
                .checked_add(1)
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "artifact tree directories",
                })?;
        if directory_count > MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "artifact tree directories",
                actual: directory_count,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES,
            });
        }
        budget
            .observe_depth(depth)
            .map_err(GenerationStoreError::Budget)?;
        let iterator = fs::read_dir(&directory).map_err(|source| {
            GenerationStoreError::io("read artifact tree directory", directory.clone(), source)
        })?;
        for entry in iterator {
            budget
                .check_entries(1)
                .and_then(|()| budget.check_members(1))
                .map_err(GenerationStoreError::Budget)?;
            let entry = entry.map_err(|source| {
                GenerationStoreError::io(
                    "read artifact tree directory entry",
                    directory.clone(),
                    source,
                )
            })?;
            budget
                .consume_entries(1)
                .and_then(|()| budget.consume_members(1))
                .map_err(GenerationStoreError::Budget)?;
            let path = entry.path();
            let metadata = metadata_no_follow(&path)?;
            if metadata.is_dir() {
                let next_depth =
                    depth
                        .checked_add(1)
                        .ok_or(GenerationStoreError::SizeOverflow {
                            resource: "artifact tree depth",
                        })?;
                budget
                    .check_depth(next_depth)
                    .map_err(GenerationStoreError::Budget)?;
                reserve_artifact_vec(
                    &mut pending,
                    1,
                    "artifact directory traversal stack",
                    budget,
                )?;
                pending.push((path, next_depth));
                continue;
            }
            if !metadata.is_file() {
                return Err(GenerationStoreError::UnsupportedFileType { path });
            }

            let bytes = metadata.len();
            total_bytes =
                total_bytes
                    .checked_add(bytes)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "artifact tree bytes",
                    })?;
            if total_bytes > MAX_PERSISTED_ARTIFACT_TREE_BYTES {
                return Err(GenerationStoreError::PersistedArtifactTooLarge {
                    artifact: "artifact tree bytes",
                    actual: total_bytes,
                    maximum: MAX_PERSISTED_ARTIFACT_TREE_BYTES,
                });
            }
            budget
                .check_bytes(bytes)
                .and_then(|()| budget.consume_bytes(bytes))
                .map_err(GenerationStoreError::Budget)?;
            let mut file = File::open(&path).map_err(|source| {
                GenerationStoreError::io("open artifact file", path.clone(), source)
            })?;
            let digest = DigestV1::hash_reader(&mut file, bytes).map_err(|source| {
                GenerationStoreError::io("hash artifact file", path.clone(), source)
            })?;
            let relative_path_bytes = portable_relative_path_byte_len(root, &path)?;
            let relative_path_bytes = u64::try_from(relative_path_bytes).map_err(|_| {
                GenerationStoreError::SizeOverflow {
                    resource: "artifact relative path bytes",
                }
            })?;
            let artifact_entry_bytes = u64::try_from(size_of::<ArtifactEntry>()).map_err(|_| {
                GenerationStoreError::SizeOverflow {
                    resource: "artifact entry allocation",
                }
            })?;
            budget
                .check_bytes(relative_path_bytes)
                .and_then(|()| budget.check_bytes(artifact_entry_bytes))
                .map_err(GenerationStoreError::Budget)?;
            reserve_artifact_vec(&mut entries, 1, "artifact tree entries", budget)?;
            budget
                .consume_bytes(relative_path_bytes)
                .and_then(|()| budget.consume_bytes(artifact_entry_bytes))
                .map_err(GenerationStoreError::Budget)?;
            let relative_path = portable_relative_path(root, &path, relative_path_bytes)?;
            entries.push(ArtifactEntry {
                relative_path,
                bytes,
                digest,
            });
        }
    }

    entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let encoded_length = artifact_tree_encoded_len(&entries)?;
    budget
        .check_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let encoded = encode_artifact_tree(&entries, encoded_length)?;
    budget
        .consume_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let file_count =
        u64::try_from(entries.len()).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "artifact file count",
        })?;
    if file_count > MAX_PERSISTED_ARTIFACT_TREE_FILES {
        return Err(GenerationStoreError::PersistedArtifactTooLarge {
            artifact: "artifact tree files",
            actual: file_count,
            maximum: MAX_PERSISTED_ARTIFACT_TREE_FILES,
        });
    }
    Ok(ArtifactTreeEvidence::new(
        DigestV1::hash_bytes(&encoded),
        file_count,
        total_bytes,
    ))
}

struct AnchoredArtifactDirectory {
    directory: SecureReadDirectory,
    listing_path: PathBuf,
    relative_path: String,
    depth: u32,
}

/// Measures a completed artifact through already-opened directory handles.
///
/// `read_dir` supplies names only. Each name is then re-opened relative to the retained
/// descriptor before metadata, hashing, or recursion occurs, so a replacement of any pathname
/// cannot redirect evidence collection outside the selected generation tree.
fn measure_anchored_artifact_tree(
    root: &Path,
    opened_root: SecureReadDirectory,
    budget: &mut AssetLoadBudget,
) -> Result<ArtifactTreeEvidence, GenerationStoreError> {
    let mut pending = Vec::new();
    reserve_artifact_vec(
        &mut pending,
        1,
        "anchored artifact directory traversal stack",
        budget,
    )?;
    pending.push(AnchoredArtifactDirectory {
        directory: opened_root,
        listing_path: root.to_path_buf(),
        relative_path: String::new(),
        depth: 0,
    });
    let mut entries = Vec::new();
    let mut directories = 0_u64;
    let mut total_bytes = 0_u64;

    while let Some(current) = pending.pop() {
        directories = directories
            .checked_add(1)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "anchored artifact tree directories",
            })?;
        if directories > MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "anchored artifact tree directories",
                actual: directories,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES,
            });
        }
        budget
            .observe_depth(current.depth)
            .map_err(GenerationStoreError::Budget)?;

        let directory_entries = fs::read_dir(&current.listing_path).map_err(|source| {
            GenerationStoreError::io(
                "enumerate anchored artifact names",
                current.listing_path.clone(),
                source,
            )
        })?;
        for listing_entry in directory_entries {
            budget
                .check_entries(1)
                .and_then(|()| budget.check_members(1))
                .map_err(GenerationStoreError::Budget)?;
            let listing_entry = listing_entry.map_err(|source| {
                GenerationStoreError::io(
                    "read anchored artifact name",
                    current.listing_path.clone(),
                    source,
                )
            })?;
            budget
                .consume_entries(1)
                .and_then(|()| budget.consume_members(1))
                .map_err(GenerationStoreError::Budget)?;

            let name = listing_entry.file_name();
            let display_path = current.listing_path.join(&name);
            let relative_path =
                anchored_relative_path(&current.relative_path, &name, &display_path, budget)?;
            let file_type = listing_entry.file_type().map_err(|source| {
                GenerationStoreError::io(
                    "inspect anchored artifact name type",
                    display_path.clone(),
                    source,
                )
            })?;

            if file_type.is_symlink() {
                return Err(persisted_link_error(display_path));
            }
            if file_type.is_dir() {
                let directory = current.directory.open_directory(&name).map_err(|source| {
                    persisted_read_error(
                        "open anchored artifact directory",
                        display_path.clone(),
                        source,
                    )
                })?;
                let depth =
                    current
                        .depth
                        .checked_add(1)
                        .ok_or(GenerationStoreError::SizeOverflow {
                            resource: "anchored artifact tree depth",
                        })?;
                budget
                    .check_depth(depth)
                    .map_err(GenerationStoreError::Budget)?;
                reserve_artifact_vec(
                    &mut pending,
                    1,
                    "anchored artifact directory traversal stack",
                    budget,
                )?;
                pending.push(AnchoredArtifactDirectory {
                    directory,
                    listing_path: display_path,
                    relative_path,
                    depth,
                });
                continue;
            }

            let file = current.directory.open_regular(&name).map_err(|source| {
                persisted_read_error("open anchored artifact file", display_path.clone(), source)
            })?;
            let bytes = file.length();
            total_bytes =
                total_bytes
                    .checked_add(bytes)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "anchored artifact tree bytes",
                    })?;
            if total_bytes > MAX_PERSISTED_ARTIFACT_TREE_BYTES {
                return Err(GenerationStoreError::PersistedArtifactTooLarge {
                    artifact: "anchored artifact tree bytes",
                    actual: total_bytes,
                    maximum: MAX_PERSISTED_ARTIFACT_TREE_BYTES,
                });
            }
            budget
                .check_bytes(bytes)
                .and_then(|()| budget.consume_bytes(bytes))
                .map_err(GenerationStoreError::Budget)?;
            let digest = DigestV1::hash_reader(
                file.range(0, bytes).map_err(|source| {
                    persisted_read_error(
                        "open anchored artifact hash range",
                        display_path.clone(),
                        source,
                    )
                })?,
                bytes,
            )
            .map_err(|source| {
                GenerationStoreError::io(
                    "hash anchored artifact file",
                    display_path.clone(),
                    source,
                )
            })?;
            file.ensure_unchanged().map_err(|source| {
                persisted_read_error(
                    "revalidate anchored artifact file",
                    display_path.clone(),
                    source,
                )
            })?;

            let entry_bytes = u64::try_from(size_of::<ArtifactEntry>()).map_err(|_| {
                GenerationStoreError::SizeOverflow {
                    resource: "anchored artifact entry allocation",
                }
            })?;
            budget
                .check_bytes(entry_bytes)
                .map_err(GenerationStoreError::Budget)?;
            reserve_artifact_vec(&mut entries, 1, "anchored artifact tree entries", budget)?;
            budget
                .consume_bytes(entry_bytes)
                .map_err(GenerationStoreError::Budget)?;
            entries.push(ArtifactEntry {
                relative_path,
                bytes,
                digest,
            });
        }
    }

    entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let encoded_length = artifact_tree_encoded_len(&entries)?;
    budget
        .check_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let encoded = encode_artifact_tree(&entries, encoded_length)?;
    budget
        .consume_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let file_count =
        u64::try_from(entries.len()).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "anchored artifact file count",
        })?;
    if file_count > MAX_PERSISTED_ARTIFACT_TREE_FILES {
        return Err(GenerationStoreError::PersistedArtifactTooLarge {
            artifact: "anchored artifact tree files",
            actual: file_count,
            maximum: MAX_PERSISTED_ARTIFACT_TREE_FILES,
        });
    }
    Ok(ArtifactTreeEvidence::new(
        DigestV1::hash_bytes(&encoded),
        file_count,
        total_bytes,
    ))
}

fn anchored_relative_path(
    parent: &str,
    name: &OsStr,
    display_path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<String, GenerationStoreError> {
    let name = name
        .to_str()
        .ok_or_else(|| GenerationStoreError::NonPortableArtifactPath {
            path: display_path.to_path_buf(),
        })?;
    let length = parent
        .len()
        .checked_add(usize::from(!parent.is_empty()))
        .and_then(|length| length.checked_add(name.len()))
        .ok_or(GenerationStoreError::SizeOverflow {
            resource: "anchored artifact relative path",
        })?;
    if length == 0 || length > MAX_ARTIFACT_RELATIVE_PATH_BYTES {
        return Err(GenerationStoreError::NonPortableArtifactPath {
            path: display_path.to_path_buf(),
        });
    }
    let bytes = u64::try_from(length).map_err(|_| GenerationStoreError::SizeOverflow {
        resource: "anchored artifact relative path bytes",
    })?;
    budget
        .check_bytes(bytes)
        .map_err(GenerationStoreError::Budget)?;
    let mut result = String::new();
    result
        .try_reserve_exact(length)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: "anchored artifact relative path",
            requested: length,
        })?;
    budget
        .consume_bytes(bytes)
        .map_err(GenerationStoreError::Budget)?;
    result.push_str(parent);
    if !parent.is_empty() {
        result.push('/');
    }
    result.push_str(name);
    Ok(result)
}

#[derive(Debug)]
struct ArtifactEntry {
    relative_path: String,
    bytes: u64,
    digest: DigestV1,
}

fn reserve_artifact_vec<T>(
    values: &mut Vec<T>,
    additional: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or(GenerationStoreError::SizeOverflow { resource })?;
    if required <= values.capacity() {
        return Ok(());
    }
    let current = values.capacity();
    let next = current
        .max(1)
        .checked_mul(2)
        .map(|capacity| capacity.max(required))
        .ok_or(GenerationStoreError::SizeOverflow { resource })?;
    let additional_capacity = next
        .checked_sub(current)
        .ok_or(GenerationStoreError::SizeOverflow { resource })?;
    let requested_bytes = additional_capacity
        .checked_mul(size_of::<T>())
        .ok_or(GenerationStoreError::SizeOverflow { resource })?;
    let budget_bytes = u64::try_from(requested_bytes)
        .map_err(|_| GenerationStoreError::SizeOverflow { resource })?;
    budget
        .check_bytes(budget_bytes)
        .map_err(GenerationStoreError::Budget)?;
    values.try_reserve_exact(additional_capacity).map_err(|_| {
        GenerationStoreError::AllocationFailed {
            resource,
            requested: requested_bytes,
        }
    })?;
    budget
        .consume_bytes(budget_bytes)
        .map_err(GenerationStoreError::Budget)
}

fn portable_relative_path_byte_len(
    root: &Path,
    path: &Path,
) -> Result<usize, GenerationStoreError> {
    let relative =
        path.strip_prefix(root)
            .map_err(|_| GenerationStoreError::ArtifactEscapedRoot {
                root: root.to_path_buf(),
                path: path.to_path_buf(),
            })?;
    let mut encoded_len = 0_usize;
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(GenerationStoreError::ArtifactEscapedRoot {
                root: root.to_path_buf(),
                path: path.to_path_buf(),
            });
        };
        let value =
            value
                .to_str()
                .ok_or_else(|| GenerationStoreError::NonPortableArtifactPath {
                    path: path.to_path_buf(),
                })?;
        encoded_len = encoded_len
            .checked_add(usize::from(encoded_len != 0))
            .and_then(|length| length.checked_add(value.len()))
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "artifact relative path",
            })?;
    }
    if encoded_len == 0 || encoded_len > MAX_ARTIFACT_RELATIVE_PATH_BYTES {
        return Err(GenerationStoreError::NonPortableArtifactPath {
            path: path.to_path_buf(),
        });
    }
    Ok(encoded_len)
}

fn portable_relative_path(
    root: &Path,
    path: &Path,
    encoded_len: u64,
) -> Result<String, GenerationStoreError> {
    let capacity =
        usize::try_from(encoded_len).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "artifact relative path bytes",
        })?;
    let relative =
        path.strip_prefix(root)
            .map_err(|_| GenerationStoreError::ArtifactEscapedRoot {
                root: root.to_path_buf(),
                path: path.to_path_buf(),
            })?;
    let mut encoded = String::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: "artifact relative path",
            requested: capacity,
        })?;
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(GenerationStoreError::ArtifactEscapedRoot {
                root: root.to_path_buf(),
                path: path.to_path_buf(),
            });
        };
        let value =
            value
                .to_str()
                .ok_or_else(|| GenerationStoreError::NonPortableArtifactPath {
                    path: path.to_path_buf(),
                })?;
        if !encoded.is_empty() {
            encoded.push('/');
        }
        encoded.push_str(value);
    }
    if encoded.len() != capacity {
        return Err(GenerationStoreError::SizeOverflow {
            resource: "artifact relative path changed length",
        });
    }
    Ok(encoded)
}

fn artifact_tree_encoded_len(entries: &[ArtifactEntry]) -> Result<u64, GenerationStoreError> {
    let mut length =
        ARTIFACT_TREE_DOMAIN
            .len()
            .checked_add(8)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "artifact tree evidence",
            })?;
    for entry in entries {
        length = length
            .checked_add(8)
            .and_then(|value| value.checked_add(entry.relative_path.len()))
            .and_then(|value| value.checked_add(8 + DigestV1::BYTE_LEN))
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "artifact tree evidence",
            })?;
    }
    u64::try_from(length).map_err(|_| GenerationStoreError::SizeOverflow {
        resource: "artifact tree evidence",
    })
}

fn encode_artifact_tree(
    entries: &[ArtifactEntry],
    encoded_length: u64,
) -> Result<Vec<u8>, GenerationStoreError> {
    let capacity =
        usize::try_from(encoded_length).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "artifact tree evidence",
        })?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: "artifact tree evidence",
            requested: capacity,
        })?;
    encoded.extend_from_slice(ARTIFACT_TREE_DOMAIN);
    encoded.extend_from_slice(
        &u64::try_from(entries.len())
            .map_err(|_| GenerationStoreError::SizeOverflow {
                resource: "artifact tree entry count",
            })?
            .to_le_bytes(),
    );
    for entry in entries {
        encoded.extend_from_slice(
            &u64::try_from(entry.relative_path.len())
                .map_err(|_| GenerationStoreError::SizeOverflow {
                    resource: "artifact relative path",
                })?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(entry.relative_path.as_bytes());
        encoded.extend_from_slice(&entry.bytes.to_le_bytes());
        encoded.extend_from_slice(entry.digest.as_bytes());
    }
    if encoded.len() != capacity {
        return Err(GenerationStoreError::SizeOverflow {
            resource: "artifact tree encoded length changed",
        });
    }
    Ok(encoded)
}

fn completed_generation_sizes(
    generations: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<(SearchGenerationId, u64)>, GenerationStoreError> {
    let mut sizes = Vec::new();
    visit_directory_entries_budgeted(generations, budget, |entry, budget| {
        let metadata = metadata_no_follow(&entry.path)?;
        if !metadata.is_dir() {
            return Ok(());
        }
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        let Some(generation) = SearchGenerationId::from_directory_name(name) else {
            return Ok(());
        };
        let bytes = tree_size_no_follow(&entry.path, budget)?;
        reserve_artifact_vec(&mut sizes, 1, "completed generation sizes", budget)?;
        sizes.push((generation, bytes));
        Ok(())
    })?;
    Ok(sizes)
}

fn generation_manifest_json_length(
    manifest: &SearchGenerationManifestV1,
    path: &Path,
) -> Result<u64, GenerationStoreError> {
    store_json_length(manifest, path, GENERATION_MANIFEST_JSON_PROFILE)
}

#[derive(Clone, Copy)]
struct StoreJsonProfile {
    artifact: &'static str,
    allocation_resource: &'static str,
    byte_resource: &'static str,
    changed_length_resource: &'static str,
    maximum_bytes: u64,
}

const GENERATION_MANIFEST_JSON_PROFILE: StoreJsonProfile = StoreJsonProfile {
    artifact: "generation manifest",
    allocation_resource: "generation manifest",
    byte_resource: "generation manifest bytes",
    changed_length_resource: "generation manifest encoded length changed",
    maximum_bytes: MAX_MANIFEST_BYTES_U64,
};

const GENERATION_HEAD_JSON_PROFILE: StoreJsonProfile = StoreJsonProfile {
    artifact: "generation head",
    allocation_resource: "generation head",
    byte_resource: "generation head bytes",
    changed_length_resource: "generation head encoded length changed",
    maximum_bytes: MAX_ACTIVATION_BYTES_U64,
};

fn store_json_length(
    value: &impl Serialize,
    path: &Path,
    profile: StoreJsonProfile,
) -> Result<u64, GenerationStoreError> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value).map_err(|source| GenerationStoreError::Json {
        artifact: profile.artifact,
        path: path.to_path_buf(),
        source,
    })?;
    if counter.bytes > profile.maximum_bytes {
        return Err(GenerationStoreError::PersistedArtifactTooLarge {
            artifact: profile.artifact,
            actual: counter.bytes,
            maximum: profile.maximum_bytes,
        });
    }
    Ok(counter.bytes)
}

fn encode_generation_manifest_json(
    manifest: &SearchGenerationManifestV1,
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, GenerationStoreError> {
    encode_store_json(manifest, path, GENERATION_MANIFEST_JSON_PROFILE, budget)
}

fn encode_generation_head_json(
    record: &GenerationHeadRecord,
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, GenerationStoreError> {
    encode_store_json(record, path, GENERATION_HEAD_JSON_PROFILE, budget)
}

fn encode_store_json(
    value: &impl Serialize,
    path: &Path,
    profile: StoreJsonProfile,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, GenerationStoreError> {
    let encoded_length = store_json_length(value, path, profile)?;
    budget
        .check_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let capacity =
        usize::try_from(encoded_length).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: profile.byte_resource,
        })?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: profile.allocation_resource,
            requested: capacity,
        })?;
    budget
        .consume_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    serde_json::to_writer(&mut encoded, value).map_err(|source| GenerationStoreError::Json {
        artifact: profile.artifact,
        path: path.to_path_buf(),
        source,
    })?;
    if encoded.len() != capacity {
        return Err(GenerationStoreError::SizeOverflow {
            resource: profile.changed_length_resource,
        });
    }
    Ok(encoded)
}

fn tree_size_no_follow(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<u64, GenerationStoreError> {
    let mut pending = managed_tree_stack(root, budget)?;
    let mut meter = ManagedTreeMeter::default();
    while let Some((directory, depth)) = pending.pop() {
        meter.observe_directory(depth, budget)?;
        visit_directory_entries_budgeted(&directory, budget, |entry, budget| {
            let metadata = metadata_no_follow(&entry.path)?;
            if metadata.is_dir() {
                push_managed_tree_directory(&mut pending, entry.path, depth, budget)?;
            } else if metadata.is_file() {
                meter.observe_file(metadata.len())?;
            } else {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            Ok(())
        })?;
    }
    Ok(meter.bytes)
}

fn sync_tree_no_follow(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let mut pending = managed_tree_stack(root, budget)?;
    let mut directories = Vec::new();
    let mut meter = ManagedTreeMeter::default();
    while let Some((directory, depth)) = pending.pop() {
        meter.observe_directory(depth, budget)?;
        visit_directory_entries_budgeted(&directory, budget, |entry, budget| {
            let metadata = metadata_no_follow(&entry.path)?;
            if metadata.is_dir() {
                push_managed_tree_directory(&mut pending, entry.path, depth, budget)?;
            } else if metadata.is_file() {
                meter.observe_file(metadata.len())?;
                sync_regular_file(&entry.path)?;
            } else {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            Ok(())
        })?;
        reserve_artifact_vec(
            &mut directories,
            1,
            "generation directories awaiting sync",
            budget,
        )?;
        directories.push((directory, depth));
    }
    directories.sort_unstable_by_key(|(_, depth)| std::cmp::Reverse(*depth));
    for (directory, _) in directories {
        sync_directory(&directory)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn sync_regular_file(path: &Path) -> Result<(), GenerationStoreError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| {
            GenerationStoreError::io("sync generation artifact", path.to_path_buf(), source)
        })
}

#[cfg(windows)]
fn sync_regular_file(path: &Path) -> Result<(), GenerationStoreError> {
    // FlushFileBuffers requires a writable handle. Generated artifacts remain
    // writable until activation; a read-only staged file is rejected explicitly.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| {
            GenerationStoreError::io("sync generation artifact", path.to_path_buf(), source)
        })
}

fn remove_tree_no_follow(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    validate_tree_no_follow(root, budget)?;
    // The standard-library implementation uses handle-relative deletion on supported Unix and
    // Windows targets, preventing a directory-to-link replacement from escaping the managed tree.
    fs::remove_dir_all(root).map_err(|source| {
        GenerationStoreError::io("remove managed generation tree", root.to_path_buf(), source)
    })
}

fn validate_tree_no_follow(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let mut pending = managed_tree_stack(root, budget)?;
    let mut meter = ManagedTreeMeter::default();
    while let Some((directory, depth)) = pending.pop() {
        meter.observe_directory(depth, budget)?;
        ensure_existing_directory_no_follow(&directory)?;
        visit_directory_entries_budgeted(&directory, budget, |entry, budget| {
            let metadata = metadata_no_follow(&entry.path)?;
            if metadata.is_dir() {
                push_managed_tree_directory(&mut pending, entry.path, depth, budget)?;
            } else if !metadata.is_file() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            } else {
                meter.observe_file(metadata.len())?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[derive(Default)]
struct ManagedTreeMeter {
    directories: u64,
    files: u64,
    bytes: u64,
}

impl ManagedTreeMeter {
    fn observe_directory(
        &mut self,
        depth: u32,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError> {
        self.directories =
            self.directories
                .checked_add(1)
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "generation tree directories",
                })?;
        if self.directories > MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "generation tree directories",
                actual: self.directories,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES,
            });
        }
        budget
            .observe_depth(depth)
            .map_err(GenerationStoreError::Budget)
    }

    fn observe_file(&mut self, bytes: u64) -> Result<(), GenerationStoreError> {
        self.files = self
            .files
            .checked_add(1)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "generation tree files",
            })?;
        if self.files > MAX_PERSISTED_ARTIFACT_TREE_FILES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "generation tree files",
                actual: self.files,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_FILES,
            });
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "generation tree bytes",
            })?;
        if self.bytes > MAX_PERSISTED_ARTIFACT_TREE_BYTES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "generation tree bytes",
                actual: self.bytes,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_BYTES,
            });
        }
        Ok(())
    }
}

fn managed_tree_stack(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<(PathBuf, u32)>, GenerationStoreError> {
    ensure_existing_directory_no_follow(root)?;
    let root_bytes =
        u64::try_from(root.as_os_str().len()).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "generation tree root path",
        })?;
    budget
        .check_bytes(root_bytes)
        .map_err(GenerationStoreError::Budget)?;
    let mut pending = Vec::new();
    reserve_artifact_vec(
        &mut pending,
        1,
        "generation directory traversal stack",
        budget,
    )?;
    budget
        .consume_bytes(root_bytes)
        .map_err(GenerationStoreError::Budget)?;
    pending.push((root.to_path_buf(), 0));
    Ok(pending)
}

fn push_managed_tree_directory(
    pending: &mut Vec<(PathBuf, u32)>,
    path: PathBuf,
    parent_depth: u32,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let depth = parent_depth
        .checked_add(1)
        .ok_or(GenerationStoreError::SizeOverflow {
            resource: "generation tree depth",
        })?;
    budget
        .check_depth(depth)
        .map_err(GenerationStoreError::Budget)?;
    reserve_artifact_vec(pending, 1, "generation directory traversal stack", budget)?;
    pending.push((path, depth));
    Ok(())
}

fn metadata_no_follow(path: &Path) -> Result<fs::Metadata, GenerationStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        GenerationStoreError::io("inspect managed entry", path.to_path_buf(), source)
    })?;
    reject_link_or_reparse(path, &metadata)?;
    Ok(metadata)
}

fn reject_link_or_reparse(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), GenerationStoreError> {
    // Windows reports junctions as symlinks too. Native reparse classification takes priority so
    // every Windows filesystem indirection has one stable error contract.
    if metadata_is_reparse_point(metadata) {
        return Err(GenerationStoreError::ReparsePoint {
            path: path.to_path_buf(),
        });
    }
    if metadata.file_type().is_symlink() {
        return Err(GenerationStoreError::Symlink {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn persisted_read_error(
    operation: &'static str,
    path: PathBuf,
    source: SecureReadError,
) -> GenerationStoreError {
    match source {
        SecureReadError::Io(source) => GenerationStoreError::io(operation, path, source),
        SecureReadError::LinkOrReparse => persisted_link_error(path),
        SecureReadError::NotDirectory => GenerationStoreError::NotDirectory { path },
        SecureReadError::NotRegular => GenerationStoreError::UnsupportedFileType { path },
        SecureReadError::IdentityChanged => GenerationStoreError::PersistedIdentityChanged { path },
    }
}

#[cfg(windows)]
fn persisted_link_error(path: PathBuf) -> GenerationStoreError {
    GenerationStoreError::ReparsePoint { path }
}

#[cfg(not(windows))]
fn persisted_link_error(path: PathBuf) -> GenerationStoreError {
    GenerationStoreError::Symlink { path }
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn path_exists_no_follow(path: &Path) -> Result<bool, GenerationStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            reject_link_or_reparse(path, &metadata)?;
            Ok(true)
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(GenerationStoreError::io(
            "inspect managed path",
            path.to_path_buf(),
            source,
        )),
    }
}

fn open_contract_file(
    directory: &SecureReadDirectory,
    file_name: &OsStr,
    path: &Path,
    maximum: u64,
    artifact: &'static str,
) -> Result<SecureRegularFile, GenerationStoreError> {
    let file = directory.open_regular(file_name).map_err(|source| {
        persisted_read_error("open persisted generation file", path.to_path_buf(), source)
    })?;
    let opened_length = file.length();
    if opened_length > maximum {
        return Err(GenerationStoreError::PersistedArtifactTooLarge {
            artifact,
            actual: opened_length,
            maximum,
        });
    }
    Ok(file)
}

struct DigestingReader<'file> {
    file: &'file mut File,
    digest: DigestV1Builder,
}

impl<'file> DigestingReader<'file> {
    fn new(file: &'file mut File, length: u64) -> Self {
        Self {
            file,
            digest: DigestV1Builder::new(length),
        }
    }

    fn finalize(self) -> Result<DigestV1, DigestBuildError> {
        self.digest.finalize()
    }
}

impl Read for DigestingReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let amount = self.file.read(output)?;
        self.digest
            .update(&output[..amount])
            .map_err(io::Error::other)?;
        Ok(amount)
    }
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), GenerationStoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            GenerationStoreError::io(
                "create persisted generation file",
                path.to_path_buf(),
                source,
            )
        })?;
    file.write_all(bytes).map_err(|source| {
        GenerationStoreError::io(
            "write persisted generation file",
            path.to_path_buf(),
            source,
        )
    })?;
    file.sync_all().map_err(|source| {
        GenerationStoreError::io("sync persisted generation file", path.to_path_buf(), source)
    })
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), GenerationStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            GenerationStoreError::io("sync generation directory", path.to_path_buf(), source)
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), GenerationStoreError> {
    // Rust's standard library has no portable API for opening and syncing directories.
    // File contents are synced explicitly; namespace durability follows the platform contract.
    Ok(())
}

fn inject_failure(
    configured: Option<GenerationFailpoint>,
    checkpoint: GenerationFailpoint,
) -> Result<(), GenerationStoreError> {
    if configured == Some(checkpoint) {
        return Err(GenerationStoreError::InjectedFailure { checkpoint });
    }
    Ok(())
}

fn checked_sum(
    values: impl IntoIterator<Item = u64>,
    resource: &'static str,
) -> Result<u64, GenerationStoreError> {
    values.into_iter().try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or(GenerationStoreError::SizeOverflow { resource })
    })
}

fn staging_directory_name(ordinal: u64) -> String {
    format!("build-{ordinal:020}")
}

fn parse_staging_directory_name(value: &str) -> Option<u64> {
    parse_ordinal_component(value, "build-", "")
}

fn quarantine_directory_name(ordinal: u64, generation: SearchGenerationId) -> String {
    format!(
        "{QUARANTINE_DIRECTORY_PREFIX}{ordinal:020}-{}",
        generation.directory_name()
    )
}

fn parse_quarantine_directory_name(value: &str) -> Option<(u64, SearchGenerationId)> {
    let encoded = value.strip_prefix(QUARANTINE_DIRECTORY_PREFIX)?;
    let ordinal = encoded.get(..ACTIVATION_FILE_DIGITS)?;
    if encoded.as_bytes().get(ACTIVATION_FILE_DIGITS) != Some(&b'-')
        || !ordinal.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let generation = SearchGenerationId::from_directory_name(
        encoded.get(ACTIVATION_FILE_DIGITS.checked_add(1)?..)?,
    )?;
    Some((ordinal.parse().ok()?, generation))
}

fn activation_file_name(ordinal: u64) -> String {
    format!("{ordinal:020}.json")
}

fn parse_activation_file_name(value: &str) -> Option<u64> {
    parse_ordinal_component(value, "", ".json")
}

fn activation_staging_file_name(ordinal: u64) -> String {
    format!("activation-{ordinal:020}.json")
}

fn parse_activation_staging_file_name(value: &str) -> Option<u64> {
    parse_ordinal_component(value, "activation-", ".json")
}

fn parse_ordinal_component(value: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let digits = value.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if digits.len() != ACTIVATION_FILE_DIGITS || !digits.bytes().all(|value| value.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

#[derive(Debug)]
pub(crate) enum SourceStateError {
    Store(Box<GenerationStoreError>),
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
    BatchTransactionsMismatch {
        batch: Vec<TransactionId>,
        receipts: Vec<TransactionId>,
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
    ManifestTransactionsMismatch,
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
        message: String,
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

impl SourceStateError {
    fn store(error: GenerationStoreError) -> Self {
        Self::Store(Box::new(error))
    }
}

impl fmt::Display for SourceStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => fmt::Display::fmt(error, formatter),
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
            Self::BatchTransactionsMismatch { batch, receipts } => write!(
                formatter,
                "analysis batch transactions {batch:?} do not match receipt transactions {receipts:?}"
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
            Self::ManifestTransactionsMismatch => formatter
                .write_str("source state transactions do not match the generation manifest"),
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
            Self::AllocationFailed { requested, message } => write!(
                formatter,
                "failed to reserve {requested} bytes for source state: {message}"
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
            Self::Store(error) => Some(error.as_ref()),
            Self::Budget(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Digest(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum GenerationStoreError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    WriterLeaseUnavailable {
        path: PathBuf,
        source: io::Error,
    },
    Json {
        artifact: &'static str,
        path: PathBuf,
        source: serde_json::Error,
    },
    ContractJson {
        artifact: &'static str,
        path: PathBuf,
        source: BudgetedJsonError,
    },
    Budget(BudgetError),
    AllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    ActivationCandidateLimitExceeded {
        maximum: usize,
    },
    Symlink {
        path: PathBuf,
    },
    ReparsePoint {
        path: PathBuf,
    },
    NotDirectory {
        path: PathBuf,
    },
    UnsupportedFileType {
        path: PathBuf,
    },
    PersistedIdentityChanged {
        path: PathBuf,
    },
    NonPortableArtifactPath {
        path: PathBuf,
    },
    ArtifactEscapedRoot {
        root: PathBuf,
        path: PathBuf,
    },
    PersistedArtifactTooLarge {
        artifact: &'static str,
        actual: u64,
        maximum: u64,
    },
    UnsupportedVersion {
        artifact: &'static str,
        actual: u16,
        expected: u16,
    },
    ActivationOrdinalMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    ActivationContextMismatch {
        generation: SearchGenerationId,
    },
    InvalidGenerationHead {
        path: PathBuf,
        message: &'static str,
    },
    ManifestDigestMismatch {
        generation: SearchGenerationId,
        expected: DigestV1,
        actual: DigestV1,
    },
    ManifestGenerationMismatch {
        expected: SearchGenerationId,
        actual: SearchGenerationId,
    },
    ArtifactEvidenceMismatch {
        expected: Box<GenerationArtifactEvidence>,
        actual: Box<GenerationArtifactEvidence>,
    },
    InvalidSourceState {
        path: PathBuf,
        message: String,
    },
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    ParentGenerationMismatch {
        expected: SearchGenerationId,
        actual: Option<SearchGenerationId>,
    },
    BuildAlreadyActive,
    ForeignBuild,
    QuarantineCollision {
        path: PathBuf,
    },
    QuarantineRollbackFailed {
        primary: Box<GenerationStoreError>,
        rollback: Box<GenerationStoreError>,
    },
    ActivationPreCommitCleanupFailed {
        primary: Box<GenerationStoreError>,
        cleanup: Box<GenerationStoreError>,
    },
    OrdinalOverflow,
    SizeOverflow {
        resource: &'static str,
    },
    InjectedFailure {
        checkpoint: GenerationFailpoint,
    },
}

impl GenerationStoreError {
    fn io(operation: &'static str, path: PathBuf, source: io::Error) -> GenerationStoreError {
        Self::Io {
            operation,
            path,
            source,
        }
    }

    fn is_security_violation(&self) -> bool {
        matches!(
            self,
            Self::Symlink { .. }
                | Self::ReparsePoint { .. }
                | Self::NotDirectory { .. }
                | Self::UnsupportedFileType { .. }
                | Self::PersistedIdentityChanged { .. }
        )
    }

    fn is_candidate_scan_fatal(&self) -> bool {
        if self.is_security_violation()
            || matches!(
                self,
                Self::Budget(_)
                    | Self::AllocationFailed { .. }
                    | Self::ActivationCandidateLimitExceeded { .. }
            )
        {
            return true;
        }
        matches!(
            self,
            Self::ContractJson { source, .. }
                if !matches!(
                    source,
                    BudgetedJsonError::Io(_)
                        | BudgetedJsonError::Json(_)
                        | BudgetedJsonError::EncodedLimitExceeded { .. }
                        | BudgetedJsonError::StructureLimitExceeded { .. }
                )
        )
    }

    fn is_repairable_completed_generation(&self) -> bool {
        matches!(
            self,
            Self::Json { .. }
                | Self::ContractJson {
                    source: BudgetedJsonError::Io(_)
                        | BudgetedJsonError::Json(_)
                        | BudgetedJsonError::EncodedLimitExceeded { .. }
                        | BudgetedJsonError::StructureLimitExceeded { .. },
                    ..
                }
                | Self::PersistedArtifactTooLarge { .. }
                | Self::ManifestGenerationMismatch { .. }
                | Self::ArtifactEvidenceMismatch { .. }
                | Self::InvalidSourceState { .. }
        ) || matches!(
            self,
            Self::Io { source, .. }
                if source.kind() == io::ErrorKind::NotFound
        )
    }

    pub(crate) fn is_retryable(&self) -> bool {
        match self {
            Self::Io { .. } | Self::WriterLeaseUnavailable { .. } => true,
            Self::QuarantineRollbackFailed { primary, rollback } => {
                primary.is_retryable() || rollback.is_retryable()
            }
            Self::ActivationPreCommitCleanupFailed { primary, cleanup } => {
                primary.is_retryable() || cleanup.is_retryable()
            }
            _ => false,
        }
    }
}

impl fmt::Display for GenerationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{operation} failed for {}: {source}",
                path.display()
            ),
            Self::WriterLeaseUnavailable { path, source } => write!(
                formatter,
                "generation writer lease is unavailable at {}: {source}",
                path.display()
            ),
            Self::Json {
                artifact,
                path,
                source,
            } => write!(
                formatter,
                "invalid {artifact} at {}: {source}",
                path.display()
            ),
            Self::ContractJson {
                artifact,
                path,
                source,
            } => write!(
                formatter,
                "invalid {artifact} at {}: {source}",
                path.display()
            ),
            Self::Budget(source) => {
                write!(formatter, "generation store budget exhausted: {source}")
            }
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to reserve {requested} bytes for {resource}"
            ),
            Self::ActivationCandidateLimitExceeded { maximum } => write!(
                formatter,
                "generation store contains more than {maximum} activation candidates"
            ),
            Self::Symlink { path } => write!(
                formatter,
                "generation store refuses symbolic link {}",
                path.display()
            ),
            Self::ReparsePoint { path } => write!(
                formatter,
                "generation store refuses Windows reparse point {}",
                path.display()
            ),
            Self::NotDirectory { path } => {
                write!(formatter, "{} is not a directory", path.display())
            }
            Self::UnsupportedFileType { path } => write!(
                formatter,
                "generation store refuses unsupported file type {}",
                path.display()
            ),
            Self::PersistedIdentityChanged { path } => write!(
                formatter,
                "persisted generation file identity, link count, or length is unsafe at {}",
                path.display()
            ),
            Self::NonPortableArtifactPath { path } => {
                write!(
                    formatter,
                    "artifact path is not portable: {}",
                    path.display()
                )
            }
            Self::ArtifactEscapedRoot { root, path } => write!(
                formatter,
                "artifact {} escaped generation root {}",
                path.display(),
                root.display()
            ),
            Self::PersistedArtifactTooLarge {
                artifact,
                actual,
                maximum,
            } => write!(
                formatter,
                "{artifact} contains {actual} bytes; maximum is {maximum}"
            ),
            Self::UnsupportedVersion {
                artifact,
                actual,
                expected,
            } => write!(
                formatter,
                "{artifact} version {actual} is unsupported; expected {expected}"
            ),
            Self::ActivationOrdinalMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "activation {} declares ordinal {actual}; filename requires {expected}",
                path.display()
            ),
            Self::ActivationContextMismatch { generation } => write!(
                formatter,
                "activation context does not match generation {generation}"
            ),
            Self::InvalidGenerationHead { path, message } => write!(
                formatter,
                "invalid generation head at {}: {message}",
                path.display()
            ),
            Self::ManifestDigestMismatch {
                generation,
                expected,
                actual,
            } => write!(
                formatter,
                "generation {generation} manifest digest mismatch: expected {expected}, got {actual}"
            ),
            Self::ManifestGenerationMismatch { expected, actual } => write!(
                formatter,
                "manifest logical generation mismatch: expected {expected}, got {actual}"
            ),
            Self::ArtifactEvidenceMismatch { expected, actual } => write!(
                formatter,
                "generation artifact evidence mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::InvalidSourceState { path, message } => write!(
                formatter,
                "invalid generation source state at {}: {message}",
                path.display()
            ),
            Self::WorkspaceMismatch { expected, actual } => write!(
                formatter,
                "generation workspace mismatch: expected {expected}, got {actual}"
            ),
            Self::ParentGenerationMismatch { expected, actual } => write!(
                formatter,
                "generation parent mismatch: expected {expected}, got {actual:?}"
            ),
            Self::BuildAlreadyActive => {
                formatter.write_str("generation store already has an armed staging build")
            }
            Self::ForeignBuild => formatter.write_str(
                "generation build does not belong to this store or has an invalid ordinal path",
            ),
            Self::QuarantineCollision { path } => write!(
                formatter,
                "generation quarantine path already exists: {}",
                path.display()
            ),
            Self::QuarantineRollbackFailed { primary, rollback } => write!(
                formatter,
                "generation publication failed ({primary}); restoring the quarantined generation also failed ({rollback})"
            ),
            Self::ActivationPreCommitCleanupFailed { primary, cleanup } => write!(
                formatter,
                "activation publication failed before commit ({primary}); activation staging cleanup also failed ({cleanup})"
            ),
            Self::OrdinalOverflow => formatter.write_str("generation ordinal overflow"),
            Self::SizeOverflow { resource } => write!(formatter, "{resource} size overflow"),
            Self::InjectedFailure { checkpoint } => {
                write!(formatter, "injected generation failure at {checkpoint:?}")
            }
        }
    }
}

impl Error for GenerationStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::WriterLeaseUnavailable { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::ContractJson { source, .. } => Some(source),
            Self::Budget(source) => Some(source),
            Self::QuarantineRollbackFailed { primary, .. }
            | Self::ActivationPreCommitCleanupFailed { primary, .. } => Some(primary.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "state/reference_generation_tests.rs"]
mod reference_generation_tests;

#[cfg(test)]
mod generation_store_tests {
    use super::*;

    #[test]
    fn anchored_artifact_measurement_honors_the_exact_caller_budget() {
        let temporary = tempfile::TempDir::new().unwrap();
        let nested = temporary.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("artifact.bin"), b"anchored artifact").unwrap();

        let mut measured = AssetLoadBudget::default();
        let evidence = measure_anchored_artifact_tree(
            temporary.path(),
            SecureReadDirectory::open(temporary.path()).unwrap(),
            &mut measured,
        )
        .unwrap();
        let usage = measured.usage();
        assert!(usage.bytes > 0);

        let exact_limits = AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..AssetLoadLimits::default()
        };
        let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
        let exact_evidence = measure_anchored_artifact_tree(
            temporary.path(),
            SecureReadDirectory::open(temporary.path()).unwrap(),
            &mut exact,
        )
        .unwrap();
        assert_eq!(exact_evidence, evidence);
        assert_eq!(exact.usage(), usage);

        let mut one_byte_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .unwrap();
        let error = measure_anchored_artifact_tree(
            temporary.path(),
            SecureReadDirectory::open(temporary.path()).unwrap(),
            &mut one_byte_short,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
    }

    #[test]
    fn activation_materialization_is_budgeted_before_typed_deserialization() {
        let temporary = tempfile::TempDir::new().unwrap();
        let path = temporary.path().join("activation.json");
        fs::write(&path, br#"{"contract_version":"invalid"}"#).unwrap();
        let directory = SecureReadDirectory::open(temporary.path()).unwrap();

        let mut measured_budget = AssetLoadBudget::default();
        let error = read_activation_record(
            &directory,
            &path,
            OsStr::new("activation.json"),
            0,
            &mut measured_budget,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::ContractJson {
                source: BudgetedJsonError::Json(_),
                ..
            }
        ));

        let usage = measured_budget.usage();
        let mut one_byte_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes.checked_sub(1).unwrap(),
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = read_activation_record(
            &directory,
            &path,
            OsStr::new("activation.json"),
            0,
            &mut one_byte_short,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::ContractJson {
                source: BudgetedJsonError::Budget(BudgetError::Exceeded {
                    resource: "bytes",
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn activation_candidate_vector_has_a_hard_limit() {
        let mut candidates = Vec::new();
        for ordinal in 0..MAX_ACTIVATION_CANDIDATES {
            push_activation_candidate(
                &mut candidates,
                ActivationCandidate {
                    ordinal: u64::try_from(ordinal).unwrap(),
                    path: PathBuf::new(),
                    file_name: OsString::new(),
                },
                None,
            )
            .unwrap();
        }

        assert!(matches!(
            push_activation_candidate(
                &mut candidates,
                ActivationCandidate {
                    ordinal: u64::try_from(MAX_ACTIVATION_CANDIDATES).unwrap(),
                    path: PathBuf::new(),
                    file_name: OsString::new(),
                },
                None,
            ),
            Err(GenerationStoreError::ActivationCandidateLimitExceeded {
                maximum: MAX_ACTIVATION_CANDIDATES
            })
        ));
    }
}

#[cfg(test)]
mod source_state_tests {
    use tempfile::TempDir;
    use unity_asset_core::{
        AssetLoadLimits, DigestV1, ObjectAddress, SourceId, SourceKind, SourceLocator,
    };
    use unity_asset_search_core::SearchKind;

    use super::*;
    use crate::analysis::{
        AnalysisTruncation, AnalysisTruncationKind, AnalyzedSource, AssetAnalysis, SearchFacts,
        WorkspaceObjectFact,
    };
    use crate::generation::{GenerationProjectionDigests, SearchGenerationIdentityV1};

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

    fn receipt_window(changes: &ChangeSet) -> TransactionReceiptWindow {
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        TransactionReceiptWindow::from_change_set(changes, &mut budget).unwrap()
    }

    fn source_state(workspace: WorkspaceId, revision: WorkspaceRevision) -> SourceStateSnapshot {
        let changes = change_set(
            workspace,
            "transaction",
            WorkspaceRevision::new(digest("previous revision")),
            revision,
            1,
        );
        SourceStateSnapshot::new(
            workspace,
            revision,
            receipt_window(&changes),
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
        assert_eq!(
            snapshot.transaction_receipts().canonical_ids(),
            vec![TransactionId::new(digest("transaction"))]
        );

        let changed_hints = SourceStateSnapshot::new(
            workspace,
            revision,
            snapshot.transaction_receipts().clone(),
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

        let mut unsupported = serde_json::to_value(&snapshot).unwrap();
        unsupported["contract_version"] = serde_json::json!(2);
        assert!(serde_json::from_value::<SourceStateSnapshot>(unsupported).is_err());
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
            TransactionReceiptMembership::Conflict { .. }
        ));
        assert!(matches!(
            receipts.append(&conflict, &mut budget),
            Err(SourceStateError::TransactionConflict { .. })
        ));
        assert_eq!(receipts.as_slice().len(), 1);
    }

    #[test]
    fn source_state_accepts_lagging_receipts_and_appends_from_snapshot_revision() {
        let workspace = WorkspaceId::from_u128(0x55).unwrap();
        let revision_0 = WorkspaceRevision::new(digest("revision 0"));
        let revision_1 = WorkspaceRevision::new(digest("revision 1"));
        let revision_2 = WorkspaceRevision::new(digest("revision 2"));
        let revision_3 = WorkspaceRevision::new(digest("revision 3"));
        let first = change_set(workspace, "transaction 1", revision_0, revision_1, 1);
        let second = change_set(workspace, "transaction 2", revision_2, revision_3, 2);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();
        let receipts = TransactionReceiptWindow::from_change_set(&first, &mut budget).unwrap();
        let snapshot =
            SourceStateSnapshot::new(workspace, revision_2, receipts, Vec::new(), Vec::new())
                .unwrap();

        assert!(matches!(
            snapshot
                .transaction_membership(&first, &mut budget)
                .unwrap(),
            TransactionReceiptMembership::Exact
        ));
        let receipts = snapshot
            .transaction_receipts_after(&second, &mut budget)
            .unwrap();
        assert_eq!(receipts.as_slice().len(), 2);
        assert_eq!(receipts.as_slice()[0].to_revision, revision_1);
        assert_eq!(receipts.as_slice()[1].from_revision, revision_2);
        SourceStateSnapshot::new(workspace, revision_3, receipts, Vec::new(), Vec::new()).unwrap();
    }

    #[test]
    fn source_state_records_reconciled_target_receipts_only_at_the_target_revision() {
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
        let reconciled = SourceStateSnapshot::new(
            workspace,
            revision_1,
            TransactionReceiptWindow::empty(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        let receipts = reconciled
            .transaction_receipts_after_reconciled_target(&changes, &mut budget)
            .unwrap();
        assert_eq!(receipts.as_slice().len(), 1);
        assert_eq!(receipts.as_slice()[0].transaction(), changes.transaction());

        let mismatched = SourceStateSnapshot::new(
            workspace,
            revision_2,
            TransactionReceiptWindow::empty(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            mismatched.transaction_receipts_after_reconciled_target(&changes, &mut budget),
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
            TransactionReceiptMembership::Absent { .. }
        ));

        let snapshot = SourceStateSnapshot::new(
            workspace,
            revisions[MAX_TRANSACTION_RECEIPTS + 1],
            receipts,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            snapshot.transaction_receipts_after(&first, &mut budget),
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

        let snapshot = SourceStateSnapshot::new(
            workspace,
            revision,
            TransactionReceiptWindow::empty(),
            Vec::new(),
            vec![asset],
        )
        .unwrap();
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
            Err(SourceStateError::Budget(BudgetedJsonError::Budget(_)))
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
            Err(SourceStateError::Budget(BudgetedJsonError::Budget(_)))
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
            Err(SourceStateError::Budget(BudgetedJsonError::Budget(_)))
        ));

        let sufficient_limits = AssetLoadLimits {
            max_bytes: read_limit.checked_add(owned_allocation).unwrap(),
            ..AssetLoadLimits::default()
        };
        let mut sufficient_budget = AssetLoadBudget::new(sufficient_limits).unwrap();
        assert!(matches!(
            read_source_state_snapshot(temporary.path(), &mut sufficient_budget, limits),
            Err(SourceStateError::Json(_))
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
        build
            .write_source_state(&snapshot, SourceStateLimits::default())
            .unwrap();
        let evidence = store.measure_artifacts(&build).unwrap();
        let identity = SearchGenerationIdentityV1::new(
            workspace,
            revision,
            GenerationProjectionDigests::new(digest("search"), digest("references")),
            Default::default(),
            None,
            snapshot.transaction_receipts().canonical_ids(),
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
            .load_source_state(&mut budget, SourceStateLimits::default())
            .unwrap();
        assert_eq!(reopened, snapshot);

        let low_limits = AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        };
        let mut low_budget = AssetLoadBudget::new(low_limits).unwrap();
        assert!(matches!(
            store
                .active()
                .unwrap()
                .load_source_state(&mut low_budget, SourceStateLimits::default()),
            Err(SourceStateError::Budget(BudgetedJsonError::Budget(_)))
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
                .load_source_state(&mut low_entry_budget, SourceStateLimits::default()),
            Err(SourceStateError::Budget(BudgetedJsonError::Budget(_)))
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
                .load_source_state(&mut low_member_budget, SourceStateLimits::default()),
            Err(SourceStateError::Budget(BudgetedJsonError::Budget(_)))
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
                .load_source_state(&mut entry_budget, entry_limits),
            Err(SourceStateError::CollectionTooLarge {
                collection: "assets",
                ..
            })
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
                .load_source_state(&mut tamper_budget, SourceStateLimits::default()),
            Err(SourceStateError::PhysicalEvidenceMismatch { .. })
        ));
    }
}
