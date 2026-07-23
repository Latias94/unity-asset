//! Deterministic recovery for durable publication journals.
//!
//! Recovery deliberately separates observation from mutation. The pure state
//! machine below decides one sticky direction from journal facts, filesystem
//! evidence, and the currently installed workspace baseline. Only after that
//! decision is durably appended may the executor move an artifact.
//!
//! The caller must authorize the locator's destination root; workspace and
//! transaction IDs provide consistency, not authentication. Owner-only state
//! and the commit guard isolate other principals and cooperating processes.
//! An actively malicious process running as the same principal remains outside
//! the filesystem race guarantee.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(all(test, unix))]
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, TransactionId, WorkspaceId, WorkspaceRevision,
    vec_allocation_bytes,
};

use super::super::portable_path::{PortablePathError, slash_key};

use super::journal::{
    BACKUP_DIRECTORY, BASELINE_DIRECTORY, EVENTS_DIRECTORY, Journal, JournalArtifact,
    JournalBaselineImage, JournalError, JournalEvent, JournalEventKey, JournalEventKind,
    JournalEventPlan, JournalLayout, JournalPath, JournalPreparation, MANIFEST_TEMPORARY_FILE,
    OpenedJournalPreparation, RECOVERY_DIRECTORY, RECOVERY_VERSION_DIRECTORY, RecoveryDirection,
    RecoveryEvidenceName, STAGE_DIRECTORY, matches_ordinal_journal_path,
    parse_recovery_evidence_name,
};
use super::platform::{
    COMMIT_LOCK_FILE, CommitGuard, CommitLockPathError, CommitLockPaths,
    DIRECTORY_VISIT_ENTRY_BYTES, DIRECTORY_VISIT_SETUP_BYTES, DirectoryEntryName,
    DirectoryIdentity, DirectoryVisitError, FileIdentity, JournalAccess, JournalDirectory,
    LEGACY_COMMIT_LOCK_DIRECTORY, SecurityMetadataError,
    capture_external_regular_in_journal_directory, capture_journal_regular,
    copy_security_metadata_between_journal_directories, journal_access, journal_directory_identity,
    observe_directory_identity, open_commit_root, open_existing_journal_namespace,
    open_journal_directory, open_journal_directory_in_directory, open_journal_regular,
    open_journal_regular_in_directory, open_readonly_regular_in_parent, opened_file_identity,
    promote_journal_regular_to_external, remove_journal_directory,
    remove_journal_directory_in_directory, remove_journal_regular,
    remove_journal_regular_in_directory, sync_journal_access, visit_existing_directory_entries,
    visit_journal_directory_entries,
};
#[cfg(test)]
use super::platform::{capture_existing, observe_file_identity};
use super::{AssetWorkspace, CommitReport, PublicationTarget, RecoveryLocator};

/// Direction selected by deterministic journal recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryDisposition {
    /// Finish publishing the exact prepared artifact set.
    Forward,
    /// Restore the complete pre-publication artifact set.
    Rollback,
    /// Preserve all evidence because neither direction is provably safe.
    Blocked,
}

/// Stable receipt for a transaction whose pre-publication state was restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackReceipt {
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    recovery: RecoveryLocator,
}

impl RollbackReceipt {
    const fn new(
        workspace_id: WorkspaceId,
        base_revision: WorkspaceRevision,
        recovery: RecoveryLocator,
    ) -> Self {
        Self {
            workspace_id,
            base_revision,
            recovery,
        }
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub const fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    #[must_use]
    pub const fn recovery(&self) -> &RecoveryLocator {
        &self.recovery
    }
}

/// Terminal result of recovering one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Publication bytes are durable, but a trusted workspace must still attach them.
    FilesystemRecovered(Box<CommitReport>),
    /// Publication bytes and the attached workspace baseline are both finalized.
    Finalized(Box<CommitReport>),
    /// A finalized journal records a historical commit, but it does not prove
    /// that its targets or an attached workspace are still current.
    HistoricalCommitReceipt(Box<CommitReport>),
    /// The pre-publication artifact set is currently verified as restored.
    RolledBack(RollbackReceipt),
    /// A finalized journal records a historical rollback, but it does not
    /// prove that its former targets are still restored.
    HistoricalRollbackReceipt(RollbackReceipt),
    /// No durable evidence exists for the supplied transaction locator.
    NoTransaction(RecoveryLocator),
}

impl RecoveryOutcome {
    #[must_use]
    pub const fn committed(&self) -> Option<&CommitReport> {
        match self {
            Self::FilesystemRecovered(report)
            | Self::Finalized(report)
            | Self::HistoricalCommitReceipt(report) => Some(report),
            Self::RolledBack(_) | Self::HistoricalRollbackReceipt(_) | Self::NoTransaction(_) => {
                None
            }
        }
    }

    #[must_use]
    pub const fn filesystem_recovered(&self) -> Option<&CommitReport> {
        match self {
            Self::FilesystemRecovered(report) => Some(report),
            Self::Finalized(_)
            | Self::HistoricalCommitReceipt(_)
            | Self::RolledBack(_)
            | Self::HistoricalRollbackReceipt(_)
            | Self::NoTransaction(_) => None,
        }
    }

    #[must_use]
    pub const fn finalized(&self) -> Option<&CommitReport> {
        match self {
            Self::Finalized(report) => Some(report),
            Self::FilesystemRecovered(_)
            | Self::HistoricalCommitReceipt(_)
            | Self::RolledBack(_)
            | Self::HistoricalRollbackReceipt(_)
            | Self::NoTransaction(_) => None,
        }
    }

    /// Returns the immutable report for a receipt that is no longer asserted
    /// to describe the current filesystem or workspace state.
    #[must_use]
    pub const fn historical_commit_receipt(&self) -> Option<&CommitReport> {
        match self {
            Self::HistoricalCommitReceipt(report) => Some(report),
            Self::FilesystemRecovered(_)
            | Self::Finalized(_)
            | Self::RolledBack(_)
            | Self::HistoricalRollbackReceipt(_)
            | Self::NoTransaction(_) => None,
        }
    }

    /// Reports whether trusted sources must be reopened before finalization.
    #[must_use]
    pub const fn requires_workspace_finalization(&self) -> bool {
        matches!(self, Self::FilesystemRecovered(_))
    }

    #[must_use]
    pub const fn rolled_back(&self) -> Option<&RollbackReceipt> {
        match self {
            Self::RolledBack(receipt) => Some(receipt),
            Self::FilesystemRecovered(_)
            | Self::Finalized(_)
            | Self::HistoricalCommitReceipt(_)
            | Self::HistoricalRollbackReceipt(_)
            | Self::NoTransaction(_) => None,
        }
    }

    /// Returns the immutable receipt for a rollback that is no longer
    /// asserted to describe the current filesystem state.
    #[must_use]
    pub const fn historical_rollback_receipt(&self) -> Option<&RollbackReceipt> {
        match self {
            Self::HistoricalRollbackReceipt(receipt) => Some(receipt),
            Self::FilesystemRecovered(_)
            | Self::Finalized(_)
            | Self::HistoricalCommitReceipt(_)
            | Self::RolledBack(_)
            | Self::NoTransaction(_) => None,
        }
    }

    #[must_use]
    pub const fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            Self::FilesystemRecovered(report)
            | Self::Finalized(report)
            | Self::HistoricalCommitReceipt(report) => Some(report.workspace_id()),
            Self::RolledBack(receipt) | Self::HistoricalRollbackReceipt(receipt) => {
                Some(receipt.workspace_id())
            }
            Self::NoTransaction(_) => None,
        }
    }

    #[must_use]
    pub const fn revision(&self) -> Option<WorkspaceRevision> {
        match self {
            Self::FilesystemRecovered(report)
            | Self::Finalized(report)
            | Self::HistoricalCommitReceipt(report) => Some(report.committed_revision()),
            Self::RolledBack(receipt) | Self::HistoricalRollbackReceipt(receipt) => {
                Some(receipt.base_revision())
            }
            Self::NoTransaction(_) => None,
        }
    }

    #[must_use]
    pub const fn recovery(&self) -> &RecoveryLocator {
        match self {
            Self::FilesystemRecovered(report)
            | Self::Finalized(report)
            | Self::HistoricalCommitReceipt(report) => report.recovery(),
            Self::RolledBack(receipt) | Self::HistoricalRollbackReceipt(receipt) => {
                receipt.recovery()
            }
            Self::NoTransaction(locator) => locator,
        }
    }
}

/// Version of the deterministic recovery-discovery response contract.
pub const RECOVERY_DISCOVERY_VERSION: u8 = 1;

/// Deterministic read-only inventory of canonical recovery candidates.
///
/// Discovery never opens a candidate journal or changes filesystem state.
/// Call [`AssetWorkspace::recover_at`] for each returned locator; that entry
/// point remains responsible for validating every journal and target before it
/// acts on the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryDiscovery {
    version: u8,
    recoveries: Vec<RecoveryLocator>,
}

impl RecoveryDiscovery {
    fn new(recoveries: Vec<RecoveryLocator>) -> Self {
        Self {
            version: RECOVERY_DISCOVERY_VERSION,
            recoveries,
        }
    }

    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.recoveries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.recoveries.len()
    }

    /// Returns unique locators sorted by their transaction identity.
    #[must_use]
    pub fn recoveries(&self) -> &[RecoveryLocator] {
        &self.recoveries
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryDiscoveryWire {
    version: u8,
    recoveries: Vec<RecoveryLocator>,
}

impl<'de> Deserialize<'de> for RecoveryDiscovery {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RecoveryDiscoveryWire::deserialize(deserializer)?;
        if wire.version != RECOVERY_DISCOVERY_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported recovery discovery response version",
            ));
        }
        if wire
            .recoveries
            .windows(2)
            .any(|pair| pair[0].transaction() >= pair[1].transaction())
        {
            return Err(serde::de::Error::custom(
                "recovery discovery locators must be strictly sorted by transaction",
            ));
        }
        Ok(Self {
            version: wire.version,
            recoveries: wire.recoveries,
        })
    }
}

/// Stable reason why recovery discovery returned no partial candidate list.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RecoveryDiscoveryBlockedReason {
    #[error("the recovery namespace contains unsupported or noncanonical evidence")]
    UnsupportedEvidence,
    #[error("the recovery namespace contains legacy transaction evidence")]
    LegacyTransactionEvidence,
    #[error("the recovery namespace contains an unsupported future protocol version")]
    FutureProtocolVersion,
    #[error("the recovery namespace could not be inspected safely")]
    UnsafeFilesystemState,
}

/// Failure to acquire or inventory the recovery namespace.
#[derive(Debug, Error)]
pub enum RecoveryDiscoveryError {
    #[error("recovery discovery is busy")]
    Busy,
    #[error("recovery discovery exceeded its caller-owned budget: {source}")]
    Budget {
        #[source]
        source: BudgetError,
    },
    #[error("recovery discovery is blocked: {reason}")]
    Blocked {
        reason: RecoveryDiscoveryBlockedReason,
    },
}

/// Stable reason why recovery preserved evidence instead of mutating it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RecoveryBlockedReason {
    #[error("the recovery locator is not a canonical transaction directory: {message}")]
    InvalidLocator { message: String },
    #[error("the durable journal is invalid: {message}")]
    InvalidJournal { message: String },
    #[error("journal artifact {artifact:?} has an unsafe {role} path")]
    UnsafePath {
        artifact: String,
        role: &'static str,
    },
    #[error("journal artifact {artifact:?} has unknown or conflicting filesystem evidence")]
    UnexpectedEvidence { artifact: String },
    #[error("the journal contains conflicting recovery decisions")]
    ConflictingDecision,
    #[error("the journal event sequence is semantically invalid: {message}")]
    InvalidEventSequence { message: String },
    #[error("journal workspace {expected} does not match the open workspace {actual}")]
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("publication bytes must be recovered before a workspace baseline can be attached")]
    FilesystemRecoveryRequired,
    #[error(
        "recovery baseline {expected} cannot be installed from this journal over current revision {actual}"
    )]
    BaselineUnavailable {
        expected: WorkspaceRevision,
        actual: WorkspaceRevision,
    },
    #[error("the published workspace baseline could not be rebuilt: {message}")]
    BaselineRebuild { message: String },
    #[error("recovery I/O evidence could not be established: {message}")]
    Io { message: String },
}

/// Failure to acquire, inspect, or complete one recovery transaction.
#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("the recovery transaction is busy: {message}")]
    Busy {
        locator: Box<RecoveryLocator>,
        message: String,
    },
    #[error("recovery is blocked: {reason}")]
    Blocked {
        locator: Box<RecoveryLocator>,
        reason: Box<RecoveryBlockedReason>,
    },
    #[error("recovery exceeded its caller-owned budget: {source}")]
    Budget {
        locator: Box<RecoveryLocator>,
        #[source]
        source: BudgetError,
    },
}

impl RecoveryError {
    #[must_use]
    pub fn locator(&self) -> &RecoveryLocator {
        match self {
            Self::Busy { locator, .. }
            | Self::Blocked { locator, .. }
            | Self::Budget { locator, .. } => locator.as_ref(),
        }
    }

    #[must_use]
    pub fn blocked_reason(&self) -> Option<&RecoveryBlockedReason> {
        match self {
            Self::Blocked { reason, .. } => Some(reason.as_ref()),
            Self::Busy { .. } | Self::Budget { .. } => None,
        }
    }
}

const MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES: usize = 128;

/// Paths retained while recovery discovery holds its read-only publication
/// guard. Every owned path is allocated through the caller-owned budget.
#[derive(Debug)]
struct DiscoveryPaths {
    recovery_root: PathBuf,
    legacy_root: PathBuf,
    version_root: PathBuf,
}

impl DiscoveryPaths {
    fn new(root: &Path, budget: &mut AssetLoadBudget) -> Result<Self, RecoveryDiscoveryError> {
        let recovery_root = budgeted_discovery_child_path(
            root,
            RECOVERY_DIRECTORY,
            "recovery discovery root path",
            budget,
        )?;
        let legacy_root = budgeted_discovery_child_path(
            &recovery_root,
            LEGACY_COMMIT_LOCK_DIRECTORY,
            "recovery discovery legacy root path",
            budget,
        )?;
        let version_root = budgeted_discovery_child_path(
            &recovery_root,
            RECOVERY_VERSION_DIRECTORY,
            "recovery discovery version root path",
            budget,
        )?;
        Ok(Self {
            recovery_root,
            legacy_root,
            version_root,
        })
    }

    fn lock_paths(
        root: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<CommitLockPaths, RecoveryDiscoveryError> {
        CommitLockPaths::new_budgeted(root, budget).map_err(|error| match error {
            CommitLockPathError::Budget(source) => RecoveryDiscoveryError::Budget { source },
            CommitLockPathError::Allocation { .. } => {
                discovery_blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState)
            }
        })
    }
}

#[derive(Debug)]
enum DiscoveryScanError {
    Budget(BudgetError),
    Blocked(RecoveryDiscoveryBlockedReason),
}

impl From<BudgetError> for DiscoveryScanError {
    fn from(source: BudgetError) -> Self {
        Self::Budget(source)
    }
}

fn discovery_scan_error(error: RecoveryDiscoveryError) -> DiscoveryScanError {
    match error {
        RecoveryDiscoveryError::Budget { source } => DiscoveryScanError::Budget(source),
        RecoveryDiscoveryError::Blocked { reason } => DiscoveryScanError::Blocked(reason),
        RecoveryDiscoveryError::Busy => {
            DiscoveryScanError::Blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState)
        }
    }
}

pub(super) fn discover_recoveries(
    target: &PublicationTarget,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryDiscovery, RecoveryDiscoveryError> {
    let paths = DiscoveryPaths::new(target.root(), budget)?;
    let lock_paths = DiscoveryPaths::lock_paths(target.root(), budget)?;
    let Some(_guard) = acquire_discovery_guard(target.root(), target.identity(), lock_paths)?
    else {
        return Ok(RecoveryDiscovery::new(Vec::new()));
    };

    let recovery_identity = discovery_directory_identity(&paths.recovery_root)?;
    let has_current_version = scan_recovery_root(&paths.recovery_root, &recovery_identity, budget)?;

    // The v1 directory is retained solely as a compatibility lock namespace.
    // Any other durable entry is legacy transaction evidence, which discovery
    // refuses to mix with a v2 candidate list.
    let legacy_identity = discovery_directory_identity(&paths.legacy_root)?;
    scan_legacy_recovery_root(&paths.legacy_root, &legacy_identity, budget)?;

    if !has_current_version {
        return Ok(RecoveryDiscovery::new(Vec::new()));
    }

    let version_identity = discovery_directory_identity(&paths.version_root)?;
    let candidate_count =
        count_v2_recovery_evidence(&paths.version_root, &version_identity, budget)?;
    let mut transactions = discovery_vec::<TransactionId>(
        candidate_count,
        "recovery discovery transaction candidates",
        budget,
    )?;
    collect_v2_recovery_evidence(
        &paths.version_root,
        &version_identity,
        candidate_count,
        &mut transactions,
        budget,
    )?;
    transactions.sort_unstable();
    transactions.dedup();

    let mut recoveries = discovery_vec::<RecoveryLocator>(
        transactions.len(),
        "recovery discovery locator list",
        budget,
    )?;
    for transaction in transactions {
        recoveries.push(discovery_recovery_locator(
            &paths.version_root,
            transaction,
            target.identity(),
            budget,
        )?);
    }
    Ok(RecoveryDiscovery::new(recoveries))
}

fn acquire_discovery_guard(
    root: &Path,
    root_identity: &DirectoryIdentity,
    paths: CommitLockPaths,
) -> Result<Option<CommitGuard>, RecoveryDiscoveryError> {
    match CommitGuard::acquire_existing(root, root_identity, paths) {
        Ok(guard) => Ok(guard),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Err(RecoveryDiscoveryError::Busy)
        }
        Err(_) => Err(discovery_blocked(
            RecoveryDiscoveryBlockedReason::UnsafeFilesystemState,
        )),
    }
}

fn discovery_directory_identity(path: &Path) -> Result<DirectoryIdentity, RecoveryDiscoveryError> {
    observe_directory_identity(path)
        .map_err(|_| discovery_blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState))
}

fn scan_recovery_root(
    root: &Path,
    identity: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<bool, RecoveryDiscoveryError> {
    let mut has_current_version = false;
    let mut scratch = [0_u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES];
    charge_discovery_directory_visit(budget)?;
    map_discovery_visit(visit_existing_directory_entries(
        root,
        identity,
        budget,
        charge_discovery_directory_entry,
        |budget, entry| {
            let name = discovery_entry_name(entry, &mut scratch)?;
            if matches!(name, "." | "..") {
                return Ok(());
            }
            charge_discovery_entry(budget)?;
            if matches!(name, COMMIT_LOCK_FILE | LEGACY_COMMIT_LOCK_DIRECTORY) {
                return Ok(());
            }
            if name == RECOVERY_VERSION_DIRECTORY {
                has_current_version = true;
                return Ok(());
            }
            if is_future_recovery_version(name) {
                return Err(DiscoveryScanError::Blocked(
                    RecoveryDiscoveryBlockedReason::FutureProtocolVersion,
                ));
            }
            Err(DiscoveryScanError::Blocked(
                RecoveryDiscoveryBlockedReason::UnsupportedEvidence,
            ))
        },
    ))?;
    Ok(has_current_version)
}

fn scan_legacy_recovery_root(
    root: &Path,
    identity: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<(), RecoveryDiscoveryError> {
    let mut scratch = [0_u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES];
    charge_discovery_directory_visit(budget)?;
    map_discovery_visit(visit_existing_directory_entries(
        root,
        identity,
        budget,
        charge_discovery_directory_entry,
        |budget, entry| {
            let name = discovery_entry_name(entry, &mut scratch)?;
            if matches!(name, "." | "..") {
                return Ok(());
            }
            charge_discovery_entry(budget)?;
            if name == COMMIT_LOCK_FILE {
                Ok(())
            } else {
                Err(DiscoveryScanError::Blocked(
                    RecoveryDiscoveryBlockedReason::LegacyTransactionEvidence,
                ))
            }
        },
    ))
}

fn count_v2_recovery_evidence(
    root: &Path,
    identity: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<usize, RecoveryDiscoveryError> {
    let mut count = 0_usize;
    let mut scratch = [0_u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES];
    charge_discovery_directory_visit(budget)?;
    map_discovery_visit(visit_existing_directory_entries(
        root,
        identity,
        budget,
        charge_discovery_directory_entry,
        |budget, entry| {
            let name = discovery_entry_name(entry, &mut scratch)?;
            if matches!(name, "." | "..") {
                return Ok(());
            }
            charge_discovery_entry(budget)?;
            let evidence = parse_recovery_evidence_name(name).ok_or(
                DiscoveryScanError::Blocked(RecoveryDiscoveryBlockedReason::UnsupportedEvidence),
            )?;
            verify_v2_evidence(root, identity, name, evidence, budget)?;
            count = count.checked_add(1).ok_or(DiscoveryScanError::Budget(
                BudgetError::ArithmeticOverflow {
                    resource: "recovery discovery candidate count",
                },
            ))?;
            Ok(())
        },
    ))?;
    Ok(count)
}

fn collect_v2_recovery_evidence(
    root: &Path,
    identity: &DirectoryIdentity,
    expected_count: usize,
    transactions: &mut Vec<TransactionId>,
    budget: &mut AssetLoadBudget,
) -> Result<(), RecoveryDiscoveryError> {
    let mut scratch = [0_u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES];
    charge_discovery_directory_visit(budget)?;
    map_discovery_visit(visit_existing_directory_entries(
        root,
        identity,
        budget,
        charge_discovery_directory_entry,
        |budget, entry| {
            let name = discovery_entry_name(entry, &mut scratch)?;
            if matches!(name, "." | "..") {
                return Ok(());
            }
            charge_discovery_entry(budget)?;
            let evidence = parse_recovery_evidence_name(name).ok_or(
                DiscoveryScanError::Blocked(RecoveryDiscoveryBlockedReason::UnsupportedEvidence),
            )?;
            verify_v2_evidence(root, identity, name, evidence, budget)?;
            if transactions.len() == expected_count {
                return Err(DiscoveryScanError::Blocked(
                    RecoveryDiscoveryBlockedReason::UnsafeFilesystemState,
                ));
            }
            transactions.push(evidence.transaction());
            Ok(())
        },
    ))?;
    if transactions.len() != expected_count {
        return Err(discovery_blocked(
            RecoveryDiscoveryBlockedReason::UnsafeFilesystemState,
        ));
    }
    Ok(())
}

fn charge_discovery_entry(budget: &mut AssetLoadBudget) -> Result<(), DiscoveryScanError> {
    budget.consume_entries(1)?;
    Ok(())
}

fn charge_discovery_directory_visit(
    budget: &mut AssetLoadBudget,
) -> Result<(), RecoveryDiscoveryError> {
    budget
        .consume_bytes(DIRECTORY_VISIT_SETUP_BYTES)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })
}

fn charge_discovery_directory_entry(
    budget: &mut AssetLoadBudget,
) -> Result<(), DiscoveryScanError> {
    budget.consume_bytes(DIRECTORY_VISIT_ENTRY_BYTES)?;
    Ok(())
}

fn discovery_entry_name<'a>(
    entry: DirectoryEntryName<'_>,
    scratch: &'a mut [u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES],
) -> Result<&'a str, DiscoveryScanError> {
    ascii_directory_entry_name(entry, scratch).ok_or(DiscoveryScanError::Blocked(
        RecoveryDiscoveryBlockedReason::UnsupportedEvidence,
    ))
}

fn ascii_directory_entry_name<'a>(
    entry: DirectoryEntryName<'_>,
    scratch: &'a mut [u8],
) -> Option<&'a str> {
    let length = entry.copy_ascii_into(scratch)?;
    std::str::from_utf8(&scratch[..length]).ok()
}

fn verify_v2_evidence(
    root: &Path,
    parent_identity: &DirectoryIdentity,
    name: &str,
    evidence: RecoveryEvidenceName,
    budget: &mut AssetLoadBudget,
) -> Result<(), DiscoveryScanError> {
    let path =
        budgeted_discovery_child_path(root, name, "recovery discovery evidence path", budget)
            .map_err(discovery_scan_error)?;
    match evidence {
        RecoveryEvidenceName::Transaction(_) => {
            if observe_directory_identity(root).map_err(|_| {
                DiscoveryScanError::Blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState)
            })? != *parent_identity
            {
                return Err(DiscoveryScanError::Blocked(
                    RecoveryDiscoveryBlockedReason::UnsafeFilesystemState,
                ));
            }
            discovery_directory_identity(&path).map_err(|_| {
                DiscoveryScanError::Blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState)
            })?;
            if observe_directory_identity(root).map_err(|_| {
                DiscoveryScanError::Blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState)
            })? != *parent_identity
            {
                return Err(DiscoveryScanError::Blocked(
                    RecoveryDiscoveryBlockedReason::UnsafeFilesystemState,
                ));
            }
            Ok(())
        }
        RecoveryEvidenceName::Preparation(_)
        | RecoveryEvidenceName::Rollback(_)
        | RecoveryEvidenceName::PreparationTemporary(_) => {
            open_readonly_regular_in_parent(&path, parent_identity).map_err(|_| {
                DiscoveryScanError::Blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState)
            })?;
            Ok(())
        }
    }
}

fn is_future_recovery_version(name: &str) -> bool {
    name.strip_prefix('v').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn map_discovery_visit(
    result: Result<(), DirectoryVisitError<DiscoveryScanError>>,
) -> Result<(), RecoveryDiscoveryError> {
    match result {
        Ok(()) => Ok(()),
        Err(DirectoryVisitError::Visitor(DiscoveryScanError::Budget(source))) => {
            Err(RecoveryDiscoveryError::Budget { source })
        }
        Err(DirectoryVisitError::Visitor(DiscoveryScanError::Blocked(reason))) => {
            Err(discovery_blocked(reason))
        }
        Err(DirectoryVisitError::Io(error)) => {
            let _ = error.kind();
            Err(discovery_blocked(
                RecoveryDiscoveryBlockedReason::UnsafeFilesystemState,
            ))
        }
    }
}

fn discovery_vec<T>(
    count: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, RecoveryDiscoveryError> {
    let planned = vec_allocation_bytes::<T>(count).map_err(|_| RecoveryDiscoveryError::Budget {
        source: BudgetError::ArithmeticOverflow { resource },
    })?;
    budget
        .check_bytes(planned)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| discovery_blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState))?;
    let actual = size_of::<T>()
        .checked_mul(values.capacity())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(RecoveryDiscoveryError::Budget {
            source: BudgetError::ArithmeticOverflow { resource },
        })?;
    budget
        .check_bytes(actual)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })?;
    budget
        .consume_bytes(actual)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })?;
    Ok(values)
}

fn discovery_recovery_locator(
    version_root: &Path,
    transaction: TransactionId,
    root_identity: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryLocator, RecoveryDiscoveryError> {
    budget
        .check_members(1)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })?;
    let slug = transaction_slug(transaction);
    let slug = std::str::from_utf8(&slug).expect("hexadecimal transaction slugs are UTF-8");
    let root = budgeted_discovery_child_path(
        version_root,
        slug,
        "recovery discovery locator path",
        budget,
    )?;
    budget
        .consume_members(1)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })?;
    Ok(RecoveryLocator::new(
        root,
        transaction,
        root_identity.clone(),
    ))
}

fn transaction_slug(transaction: TransactionId) -> [u8; DigestV1::BYTE_LEN * 2] {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut slug = [0_u8; DigestV1::BYTE_LEN * 2];
    for (index, byte) in transaction.digest().as_bytes().iter().copied().enumerate() {
        slug[index * 2] = HEX[usize::from(byte >> 4)];
        slug[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
    }
    slug
}

fn budgeted_discovery_child_path(
    parent: &Path,
    child: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, RecoveryDiscoveryError> {
    let requested = parent
        .as_os_str()
        .len()
        .checked_add(child.len())
        .and_then(|capacity| capacity.checked_add(1))
        .ok_or(RecoveryDiscoveryError::Budget {
            source: BudgetError::ArithmeticOverflow { resource },
        })?;
    let requested = u64::try_from(requested).map_err(|_| RecoveryDiscoveryError::Budget {
        source: BudgetError::ArithmeticOverflow { resource },
    })?;
    budget
        .check_bytes(requested)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })?;

    let mut value = OsString::new();
    value
        .try_reserve_exact(usize::try_from(requested).map_err(|_| {
            RecoveryDiscoveryError::Budget {
                source: BudgetError::ArithmeticOverflow { resource },
            }
        })?)
        .map_err(|_| discovery_blocked(RecoveryDiscoveryBlockedReason::UnsafeFilesystemState))?;
    value.push(parent.as_os_str());
    let mut path = PathBuf::from(value);
    path.push(child);
    let capacity = u64::try_from(path.capacity()).map_err(|_| RecoveryDiscoveryError::Budget {
        source: BudgetError::ArithmeticOverflow { resource },
    })?;
    budget
        .check_bytes(capacity)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })?;
    budget
        .consume_bytes(capacity)
        .map_err(|source| RecoveryDiscoveryError::Budget { source })?;
    Ok(path)
}

fn discovery_blocked(reason: RecoveryDiscoveryBlockedReason) -> RecoveryDiscoveryError {
    RecoveryDiscoveryError::Blocked { reason }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryEvidence {
    Missing,
    Old,
    New,
    CorruptOld,
    CorruptNew,
    Unexpected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArtifactObservation {
    target: EntryEvidence,
    staging: EntryEvidence,
    backup: EntryEvidence,
    had_original: bool,
}

impl ArtifactObservation {
    const fn can_forward(self) -> bool {
        if self.had_original {
            matches!(
                (self.target, self.staging, self.backup),
                (
                    EntryEvidence::Old,
                    EntryEvidence::New,
                    EntryEvidence::Missing
                ) | (
                    EntryEvidence::Missing,
                    EntryEvidence::New,
                    EntryEvidence::Old
                ) | (
                    EntryEvidence::New,
                    EntryEvidence::Missing,
                    EntryEvidence::Old
                )
            )
        } else {
            matches!(
                (self.target, self.staging, self.backup),
                (
                    EntryEvidence::Missing,
                    EntryEvidence::New,
                    EntryEvidence::Missing
                ) | (
                    EntryEvidence::New,
                    EntryEvidence::Missing,
                    EntryEvidence::Missing
                )
            )
        }
    }

    const fn can_rollback(self) -> bool {
        if self.had_original {
            matches!(
                (self.target, self.staging, self.backup),
                (
                    EntryEvidence::Old,
                    EntryEvidence::Missing | EntryEvidence::New,
                    EntryEvidence::Missing
                ) | (
                    EntryEvidence::Missing,
                    EntryEvidence::Missing | EntryEvidence::New,
                    EntryEvidence::Old
                ) | (
                    EntryEvidence::New | EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::Old
                ) | (
                    EntryEvidence::Missing,
                    EntryEvidence::Missing | EntryEvidence::New,
                    EntryEvidence::CorruptOld
                ) | (
                    EntryEvidence::New | EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::CorruptOld
                )
            )
        } else {
            matches!(
                (self.target, self.staging, self.backup),
                (
                    EntryEvidence::Missing,
                    EntryEvidence::Missing | EntryEvidence::New,
                    EntryEvidence::Missing
                ) | (
                    EntryEvidence::New | EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::Missing
                )
            )
        }
    }

    fn is_published(self) -> bool {
        self.target == EntryEvidence::New
            && self.staging == EntryEvidence::Missing
            && if self.had_original {
                self.backup == EntryEvidence::Old
            } else {
                self.backup == EntryEvidence::Missing
            }
    }

    fn is_rolled_back(self) -> bool {
        let target_matches = if self.had_original {
            self.target == EntryEvidence::Old
        } else {
            self.target == EntryEvidence::Missing
        };
        target_matches
            && self.staging != EntryEvidence::Unexpected
            && self.backup == EntryEvidence::Missing
    }

    fn contains_unexpected(self) -> bool {
        self.target == EntryEvidence::Unexpected
            || self.staging == EntryEvidence::Unexpected
            || self.backup == EntryEvidence::Unexpected
    }

    fn contains_corrupt_owned(self) -> bool {
        matches!(
            self.target,
            EntryEvidence::CorruptOld | EntryEvidence::CorruptNew
        ) || matches!(
            self.staging,
            EntryEvidence::CorruptOld | EntryEvidence::CorruptNew
        ) || matches!(
            self.backup,
            EntryEvidence::CorruptOld | EntryEvidence::CorruptNew
        )
    }

    fn has_repairable_owned_corruption(self) -> bool {
        if self.had_original {
            matches!(
                (self.target, self.staging, self.backup),
                (
                    EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::Old | EntryEvidence::CorruptOld
                ) | (
                    EntryEvidence::New,
                    EntryEvidence::Missing,
                    EntryEvidence::CorruptOld
                ) | (
                    EntryEvidence::Missing,
                    EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                    EntryEvidence::CorruptOld
                ) | (
                    EntryEvidence::Missing,
                    EntryEvidence::CorruptNew,
                    EntryEvidence::Old
                )
            )
        } else {
            matches!(
                (self.target, self.staging, self.backup),
                (
                    EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::Missing
                )
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaselineObservation {
    Base,
    Committed,
    Other,
    Detached,
}

#[derive(Debug, Clone, Default)]
struct ArtifactEventFacts {
    backup_intent: bool,
    backup_captured: bool,
    promotion_intent: bool,
    promoted: bool,
}

#[derive(Debug, Clone, Default)]
struct EventFacts {
    staging_verified: bool,
    journaled: bool,
    artifacts: Vec<ArtifactEventFacts>,
    published: bool,
    baseline_installed: bool,
    abandoned: bool,
    finalized: bool,
    direction: Option<RecoveryDirection>,
    blocked_reason: Option<String>,
}

#[derive(Debug)]
struct RecoveryObservation {
    events: EventFacts,
    artifacts: Vec<ArtifactObservation>,
    baseline: BaselineObservation,
}

/// All paths and fixed metadata required after recovery chooses a durable
/// direction. Constructing this plan is an explicitly budgeted pre-decision
/// operation, so forward and rollback execution do not allocate path state.
#[derive(Debug)]
struct RecoveryExecutionPlan {
    artifacts: Vec<RecoveryArtifactExecution>,
}

#[derive(Debug)]
struct RecoveryArtifactExecution {
    ordinal: u32,
    target: PathBuf,
    staging: PathBuf,
    backup: Option<PathBuf>,
    target_parent_identity: DirectoryIdentity,
    old_digest: Option<DigestV1>,
    old_identity: Option<FileIdentity>,
    new_digest: DigestV1,
    new_identity: FileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryPlan {
    disposition: RecoveryDisposition,
    blocked: Option<RecoveryBlockedReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryIntent {
    Resume,
    Abandon,
}

impl RecoveryPlan {
    const fn forward() -> Self {
        Self {
            disposition: RecoveryDisposition::Forward,
            blocked: None,
        }
    }

    const fn rollback() -> Self {
        Self {
            disposition: RecoveryDisposition::Rollback,
            blocked: None,
        }
    }

    fn blocked(reason: RecoveryBlockedReason) -> Self {
        Self {
            disposition: RecoveryDisposition::Blocked,
            blocked: Some(reason),
        }
    }
}

fn decide_recovery(observation: &RecoveryObservation) -> RecoveryPlan {
    if let Some(reason) = &observation.events.blocked_reason {
        return RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
            message: format!("a previous recovery was blocked: {reason}"),
        });
    }
    if let Some((index, _)) = observation
        .artifacts
        .iter()
        .enumerate()
        .find(|(_, artifact)| artifact.contains_unexpected())
    {
        return RecoveryPlan::blocked(RecoveryBlockedReason::UnexpectedEvidence {
            artifact: format!("artifact-{index:08}"),
        });
    }

    if observation.events.abandoned {
        return if observation
            .artifacts
            .iter()
            .all(|artifact| artifact.is_rolled_back())
            && matches!(
                observation.baseline,
                BaselineObservation::Base | BaselineObservation::Detached
            ) {
            RecoveryPlan::rollback()
        } else {
            RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
                message: "an abandoned transaction is not fully rolled back".to_owned(),
            })
        };
    }

    if observation.events.published
        || observation.events.baseline_installed
        || observation.events.finalized
    {
        if let Some((index, _)) = observation
            .artifacts
            .iter()
            .enumerate()
            .find(|(_, artifact)| artifact.contains_corrupt_owned())
        {
            return RecoveryPlan::blocked(RecoveryBlockedReason::UnexpectedEvidence {
                artifact: format!("artifact-{index:08}"),
            });
        }
        if !observation
            .artifacts
            .iter()
            .all(|artifact| artifact.is_published())
        {
            return RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
                message: "a published transaction does not retain every new artifact".to_owned(),
            });
        }
        return RecoveryPlan::forward();
    }

    match observation.events.direction {
        Some(RecoveryDirection::Forward) => {
            if observation
                .artifacts
                .iter()
                .all(|artifact| artifact.can_forward())
            {
                RecoveryPlan::forward()
            } else {
                RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
                    message: "the sticky forward decision no longer has complete evidence"
                        .to_owned(),
                })
            }
        }
        Some(RecoveryDirection::Rollback) => {
            if observation
                .artifacts
                .iter()
                .all(|artifact| artifact.can_rollback())
            {
                RecoveryPlan::rollback()
            } else {
                RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
                    message: "the sticky rollback decision no longer has complete evidence"
                        .to_owned(),
                })
            }
        }
        None if observation
            .artifacts
            .iter()
            .all(|artifact| artifact.can_forward()) =>
        {
            RecoveryPlan::forward()
        }
        None if observation
            .artifacts
            .iter()
            .all(|artifact| artifact.can_rollback()) =>
        {
            RecoveryPlan::rollback()
        }
        None => {
            if let Some((index, _)) = observation
                .artifacts
                .iter()
                .enumerate()
                .find(|(_, artifact)| artifact.contains_corrupt_owned())
            {
                RecoveryPlan::blocked(RecoveryBlockedReason::UnexpectedEvidence {
                    artifact: format!("artifact-{index:08}"),
                })
            } else {
                RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
                    message: "neither forward publication nor rollback has complete evidence"
                        .to_owned(),
                })
            }
        }
    }
}

fn decide_abandon(observation: &RecoveryObservation) -> RecoveryPlan {
    if let Some(reason) = &observation.events.blocked_reason {
        return RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
            message: format!("a previous recovery was blocked: {reason}"),
        });
    }
    if let Some((index, _)) = observation
        .artifacts
        .iter()
        .enumerate()
        .find(|(_, artifact)| artifact.contains_unexpected())
    {
        return RecoveryPlan::blocked(RecoveryBlockedReason::UnexpectedEvidence {
            artifact: format!("artifact-{index:08}"),
        });
    }
    if observation.events.abandoned {
        return if observation
            .artifacts
            .iter()
            .all(|artifact| artifact.is_rolled_back())
            && observation.baseline == BaselineObservation::Base
        {
            RecoveryPlan::rollback()
        } else {
            RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
                message: "an abandoned transaction is not fully rolled back".to_owned(),
            })
        };
    }
    if observation.events.published
        || observation.events.baseline_installed
        || observation.events.finalized
    {
        return RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
            message: "a published or finalized transaction cannot be explicitly abandoned"
                .to_owned(),
        });
    }
    if observation.events.direction == Some(RecoveryDirection::Forward) {
        return RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
            message: "a transaction with a sticky forward decision cannot be abandoned".to_owned(),
        });
    }
    if !matches!(
        observation.baseline,
        BaselineObservation::Base | BaselineObservation::Detached
    ) {
        return RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
            message: "explicit abandon requires the workspace base revision".to_owned(),
        });
    }
    if observation
        .artifacts
        .iter()
        .all(|artifact| artifact.can_rollback())
    {
        RecoveryPlan::rollback()
    } else {
        RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
            message: "transaction evidence cannot be safely rolled back for explicit abandon"
                .to_owned(),
        })
    }
}

fn recovery_event_keys(
    observation: &RecoveryObservation,
    disposition: RecoveryDisposition,
    finalize_workspace: bool,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<JournalEventKey>, ObservationError> {
    let capacity = observation
        .artifacts
        .len()
        .checked_mul(4)
        .and_then(|events| events.checked_add(6))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "recovery event plan keys",
        })?;
    let mut keys = recovery_vec(capacity, "recovery event plan keys", budget)?;
    let direction = match disposition {
        RecoveryDisposition::Forward => RecoveryDirection::Forward,
        RecoveryDisposition::Rollback => RecoveryDirection::Rollback,
        RecoveryDisposition::Blocked => return Ok(keys),
    };
    if observation.events.direction.is_none() {
        keys.push(JournalEventKey::RecoveryDecision(direction));
    }
    if disposition == RecoveryDisposition::Rollback {
        if !observation.events.abandoned {
            keys.push(JournalEventKey::Abandoned);
        }
        if !observation.events.finalized {
            keys.push(JournalEventKey::Finalized);
        }
        return Ok(keys);
    }

    if !observation.events.staging_verified {
        keys.push(JournalEventKey::StagingVerified);
    }
    if !observation.events.journaled {
        keys.push(JournalEventKey::Journaled);
    }
    let already_published = observation
        .artifacts
        .iter()
        .all(|artifact| artifact.is_published());
    for (index, (artifact, facts)) in observation
        .artifacts
        .iter()
        .zip(&observation.events.artifacts)
        .enumerate()
    {
        let ordinal = u32::try_from(index).map_err(|_| RecoveryBlockedReason::InvalidJournal {
            message: "recovery artifact ordinal overflowed".to_owned(),
        })?;
        if already_published {
            if artifact.had_original && !facts.backup_captured {
                return Err(
                    invalid_event("promoted replacement has no captured backup event").into(),
                );
            }
            if !facts.promotion_intent {
                return Err(invalid_event("promoted target has no durable intent").into());
            }
            if !facts.promoted {
                keys.push(JournalEventKey::Promoted(ordinal));
            }
            continue;
        }

        if artifact.had_original {
            match (artifact.target, artifact.staging, artifact.backup) {
                (EntryEvidence::Old, EntryEvidence::New, EntryEvidence::Missing) => {
                    if !facts.backup_intent {
                        keys.push(JournalEventKey::BackupIntent(ordinal));
                    }
                    if !facts.backup_captured {
                        keys.push(JournalEventKey::BackupCaptured(ordinal));
                    }
                }
                (EntryEvidence::Missing, EntryEvidence::New, EntryEvidence::Old) => {
                    if !facts.backup_intent {
                        return Err(invalid_event("captured backup has no durable intent").into());
                    }
                    if !facts.backup_captured {
                        keys.push(JournalEventKey::BackupCaptured(ordinal));
                    }
                }
                (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::Old) => {
                    if !facts.promotion_intent {
                        return Err(invalid_event("promoted target has no durable intent").into());
                    }
                    if !facts.promoted {
                        keys.push(JournalEventKey::Promoted(ordinal));
                    }
                    continue;
                }
                _ => {
                    return Err(RecoveryBlockedReason::UnexpectedEvidence {
                        artifact: format!("artifact-{index:08}"),
                    }
                    .into());
                }
            }
        } else {
            match (artifact.target, artifact.staging, artifact.backup) {
                (EntryEvidence::Missing, EntryEvidence::New, EntryEvidence::Missing) => {}
                (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::Missing) => {
                    if !facts.promotion_intent {
                        return Err(invalid_event("promoted target has no durable intent").into());
                    }
                    if !facts.promoted {
                        keys.push(JournalEventKey::Promoted(ordinal));
                    }
                    continue;
                }
                _ => {
                    return Err(RecoveryBlockedReason::UnexpectedEvidence {
                        artifact: format!("artifact-{index:08}"),
                    }
                    .into());
                }
            }
        }
        if !facts.promotion_intent {
            keys.push(JournalEventKey::PromotionIntent(ordinal));
        }
        if !facts.promoted {
            keys.push(JournalEventKey::Promoted(ordinal));
        }
    }
    if !observation.events.published {
        keys.push(JournalEventKey::Published);
    }
    if finalize_workspace {
        if !observation.events.baseline_installed {
            keys.push(JournalEventKey::BaselineInstalled);
        }
        if !observation.events.finalized {
            keys.push(JournalEventKey::Finalized);
        }
    }
    Ok(keys)
}

fn prebuild_recovery_baseline(
    workspace: &AssetWorkspace,
    journal: &Journal,
    observations: &[ArtifactObservation],
    locator: &RecoveryLocator,
    budget: &mut AssetLoadBudget,
) -> Result<super::baseline::PreparedBaseline, RecoveryError> {
    if observations.len() != journal.manifest().artifacts().len() {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::InvalidJournal {
                message: "recovery artifact observations are incomplete".to_owned(),
            },
        ));
    }
    let mut images = recovery_vec(
        observations.len(),
        "recovery prepublication artifact images",
        budget,
    )
    .map_err(|error| map_observation_error(locator, error))?;
    for (index, observation) in observations.iter().enumerate() {
        let location = if observation.target == EntryEvidence::New {
            super::baseline::RecoveryArtifactLocation::Target
        } else if observation.staging == EntryEvidence::New {
            super::baseline::RecoveryArtifactLocation::Staging
        } else {
            return Err(blocked(
                locator,
                RecoveryBlockedReason::UnexpectedEvidence {
                    artifact: format!("artifact-{index:08}"),
                },
            ));
        };
        let image = super::baseline::read_artifact_image(journal, index, location, budget)
            .map_err(|error| map_baseline_error(locator, error))?;
        images.push(Some(image));
    }
    let expected = Arc::clone(workspace.state());
    super::baseline::build_from_journal_with_images(
        expected,
        journal,
        workspace.binary_adapter(),
        Some(&images),
        budget,
    )
    .map_err(|error| map_baseline_error(locator, error))
}

impl AssetWorkspace {
    /// Recovers publication bytes before any workspace sources are opened.
    ///
    /// This entry point trusts only the caller-provided recovery locator. It
    /// never treats journal data as authority to open source paths outside the
    /// locator's containment root. Callers may reopen their trusted source
    /// requests with the workspace identity returned in the canonical report,
    /// then call [`Self::finalize_recovery_at`] to finalize the in-memory
    /// baseline. An unfinished committed result is returned as
    /// [`RecoveryOutcome::FilesystemRecovered`] until that second stage
    /// succeeds. A journal that was already finalized is immutable historical
    /// evidence, so this detached entry point returns
    /// [`RecoveryOutcome::HistoricalCommitReceipt`] instead of asserting that
    /// its former targets are still current.
    pub fn recover_at(
        locator: &RecoveryLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        recover_with_intent(None, locator, budget, RecoveryIntent::Resume)
    }

    /// Rolls back an unfinished publication before opening workspace sources.
    pub fn abandon_at(
        locator: &RecoveryLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        recover_with_intent(None, locator, budget, RecoveryIntent::Abandon)
    }

    /// Attaches a filesystem recovery to an already reopened trusted workspace.
    ///
    /// Call [`Self::recover_at`] or [`Self::abandon_at`] first, create a workspace
    /// with the returned identity, and reopen source requests from caller-owned
    /// trusted configuration. This method never finishes pending filesystem
    /// renames; it rejects them with
    /// [`RecoveryBlockedReason::FilesystemRecoveryRequired`]. A successful
    /// current committed result is returned as [`RecoveryOutcome::Finalized`].
    /// If the attached workspace has advanced, or a finalized receipt's
    /// targets no longer match its artifact set, the immutable receipt is
    /// returned as [`RecoveryOutcome::HistoricalCommitReceipt`] without
    /// replacing current state. Rolled back and absent transactions require no
    /// workspace finalization.
    pub fn finalize_recovery_at(
        &mut self,
        locator: &RecoveryLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        recover_with_intent(Some(self), locator, budget, RecoveryIntent::Resume)
    }
}

fn recover_with_intent(
    mut workspace: Option<&mut AssetWorkspace>,
    locator: &RecoveryLocator,
    budget: &mut AssetLoadBudget,
    intent: RecoveryIntent,
) -> Result<RecoveryOutcome, RecoveryError> {
    let layout = layout_from_locator(locator, budget)?;
    let root = open_commit_root(layout.parent(), layout.root_identity())
        .map_err(|error| map_commit_guard_error(locator, error))?;
    let _guard = CommitGuard::acquire_with_root(&root)
        .map_err(|error| map_commit_guard_error(locator, error))?;
    #[cfg(all(test, unix))]
    test_run_recovery_post_guard_hook(locator);
    layout.verify_root_path_binding().map_err(|error| {
        blocked(
            locator,
            RecoveryBlockedReason::InvalidLocator {
                message: error.to_string(),
            },
        )
    })?;
    let namespace = match open_existing_journal_namespace(&root) {
        Ok(namespace) => namespace,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RecoveryOutcome::NoTransaction(locator.clone()));
        }
        Err(error) => return Err(blocked(locator, io_reason(error))),
    };
    let access = journal_access(&root, &namespace);
    match open_journal_directory(&access, layout.directory()) {
        Ok(directory) => {
            match open_journal_regular_in_directory(&directory, layout.manifest_path()) {
                Ok(_) => {
                    let mut journal = Journal::open_in_access(layout, &access, budget)
                        .map_err(|error| map_journal_open_error(locator, error))?;
                    recover_open_journal(
                        workspace.as_deref_mut(),
                        &mut journal,
                        locator,
                        intent,
                        budget,
                    )
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    recover_prepared_transaction(
                        workspace.as_deref(),
                        &layout,
                        locator,
                        &access,
                        budget,
                    )
                }
                Err(_) => Err(blocked(
                    locator,
                    invalid_journal("canonical manifest is not a regular file".to_owned()),
                )),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            recover_prepared_transaction(workspace.as_deref(), &layout, locator, &access, budget)
        }
        Err(error) => Err(blocked(locator, io_reason(error))),
    }
}

#[cfg(all(test, unix))]
struct RecoveryPostGuardHook {
    root: PathBuf,
    action: Box<dyn FnOnce() + Send>,
}

#[cfg(all(test, unix))]
fn recovery_post_guard_hook() -> &'static Mutex<Option<RecoveryPostGuardHook>> {
    static HOOK: OnceLock<Mutex<Option<RecoveryPostGuardHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(all(test, unix))]
fn test_install_recovery_post_guard_hook(root: PathBuf, action: impl FnOnce() + Send + 'static) {
    let mut hook = recovery_post_guard_hook()
        .lock()
        .expect("recovery post-guard hook lock");
    assert!(
        hook.is_none(),
        "a recovery post-guard hook is already installed"
    );
    *hook = Some(RecoveryPostGuardHook {
        root,
        action: Box::new(action),
    });
}

#[cfg(all(test, unix))]
fn test_run_recovery_post_guard_hook(locator: &RecoveryLocator) {
    let action = {
        let mut hook = recovery_post_guard_hook()
            .lock()
            .expect("recovery post-guard hook lock");
        if hook
            .as_ref()
            .is_some_and(|installed| installed.root == locator.root())
        {
            hook.take().map(|installed| installed.action)
        } else {
            None
        }
    };
    if let Some(action) = action {
        action();
    }
}

fn recover_prepared_transaction(
    workspace: Option<&AssetWorkspace>,
    layout: &JournalLayout,
    locator: &RecoveryLocator,
    access: &JournalAccess<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryOutcome, RecoveryError> {
    match JournalPreparation::open_rollback_in_access(layout, access, budget) {
        Ok(_) => return recover_premanifest_rollback(workspace, layout, locator, access, budget),
        Err(JournalError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_journal_open_error(locator, error)),
    }
    let preparation = match JournalPreparation::open_in_access(layout, access, budget) {
        Ok(preparation) => preparation,
        Err(JournalError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return recover_absent_prepared_transaction(layout, locator, access, budget);
        }
        Err(error) => return Err(map_journal_open_error(locator, error)),
    };
    if let Some(workspace) = workspace
        && preparation.document().workspace_id() != workspace.workspace_id()
    {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::WorkspaceMismatch {
                expected: preparation.document().workspace_id(),
                actual: workspace.workspace_id(),
            },
        ));
    }
    let receipt = RollbackReceipt::new(
        preparation.document().workspace_id(),
        preparation.document().base_revision(),
        locator.clone(),
    );
    let plan = observe_premanifest_cleanup(layout, access, preparation, budget)
        .map_err(|error| map_observation_error(locator, error))?;
    execute_premanifest_cleanup(
        layout,
        access,
        &plan,
        PreparationCleanup::RetainRollbackReceipt,
    )
    .map_err(|error| {
        blocked(
            locator,
            RecoveryBlockedReason::Io {
                message: error.to_string(),
            },
        )
    })?;
    Ok(RecoveryOutcome::RolledBack(receipt))
}

fn recover_premanifest_rollback(
    workspace: Option<&AssetWorkspace>,
    layout: &JournalLayout,
    locator: &RecoveryLocator,
    access: &JournalAccess<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryOutcome, RecoveryError> {
    let rollback = JournalPreparation::open_rollback_in_access(layout, access, budget)
        .map_err(|error| map_journal_open_error(locator, error))?;
    let duplicate_preparation = match JournalPreparation::open_in_access(layout, access, budget) {
        Ok(preparation) => {
            if preparation.document() != rollback.document() {
                return Err(blocked(
                    locator,
                    unexpected_premanifest("mismatched-active-preparation-record"),
                ));
            }
            Some(preparation)
        }
        Err(JournalError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(map_journal_open_error(locator, error)),
    };
    ensure_premanifest_rollback_absence(access, layout, duplicate_preparation.is_none())
        .map_err(|reason| blocked(locator, reason))?;
    if let Some(workspace) = workspace {
        if rollback.document().workspace_id() != workspace.workspace_id() {
            return Err(blocked(
                locator,
                RecoveryBlockedReason::WorkspaceMismatch {
                    expected: rollback.document().workspace_id(),
                    actual: workspace.workspace_id(),
                },
            ));
        }
        if rollback.document().base_revision() != workspace.revision() {
            return Err(blocked(
                locator,
                RecoveryBlockedReason::BaselineUnavailable {
                    expected: rollback.document().base_revision(),
                    actual: workspace.revision(),
                },
            ));
        }
    }
    if let Some(preparation) = duplicate_preparation {
        let current = JournalPreparation::open_in_access(layout, access, budget)
            .map_err(|error| map_journal_open_error(locator, error))?;
        if current.identity() != preparation.identity()
            || current.document() != preparation.document()
        {
            return Err(blocked(
                locator,
                unexpected_premanifest("changed-active-preparation-record"),
            ));
        }
        let current_rollback = JournalPreparation::open_rollback_in_access(layout, access, budget)
            .map_err(|error| map_journal_open_error(locator, error))?;
        if current_rollback.identity() != rollback.identity()
            || current_rollback.document() != rollback.document()
        {
            return Err(blocked(
                locator,
                unexpected_premanifest("changed-rollback-record"),
            ));
        }
        remove_journal_regular(access, layout.preparation_path(), preparation.identity())
            .map_err(|error| map_journal_open_error(locator, JournalError::Io(error)))?;
        sync_journal_access(access)
            .map_err(|error| map_journal_open_error(locator, JournalError::Io(error)))?;
        ensure_premanifest_rollback_absence(access, layout, true)
            .map_err(|reason| blocked(locator, reason))?;
        let current_rollback = JournalPreparation::open_rollback_in_access(layout, access, budget)
            .map_err(|error| map_journal_open_error(locator, error))?;
        if current_rollback.identity() != rollback.identity()
            || current_rollback.document() != rollback.document()
        {
            return Err(blocked(
                locator,
                unexpected_premanifest("changed-rollback-record"),
            ));
        }
    }
    Ok(RecoveryOutcome::RolledBack(RollbackReceipt::new(
        rollback.document().workspace_id(),
        rollback.document().base_revision(),
        locator.clone(),
    )))
}

fn ensure_premanifest_rollback_absence(
    access: &JournalAccess<'_>,
    layout: &JournalLayout,
    require_active_preparation_absent: bool,
) -> Result<(), RecoveryBlockedReason> {
    if require_active_preparation_absent {
        match open_journal_regular(access, layout.preparation_path()) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) | Err(_) => {
                return Err(unexpected_premanifest("active-preparation-record"));
            }
        }
    }
    match open_journal_regular(access, layout.preparation_temporary_path()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) | Err(_) => {
            return Err(unexpected_premanifest("preparation-temporary-record"));
        }
    }
    match open_journal_directory(access, layout.directory()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) | Err(_) => Err(unexpected_premanifest("transaction-directory")),
    }
}

fn recover_absent_prepared_transaction(
    layout: &JournalLayout,
    locator: &RecoveryLocator,
    access: &JournalAccess<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryOutcome, RecoveryError> {
    match open_journal_directory(access, layout.directory()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(blocked(
                locator,
                RecoveryBlockedReason::InvalidJournal {
                    message: "transaction state exists without its durable preparation record"
                        .to_owned(),
                },
            ));
        }
        Err(error) => return Err(blocked(locator, io_reason(error))),
    }
    cleanup_orphaned_preparation_attempts(layout, access, budget)
        .map_err(|error| map_premanifest_cleanup_error(locator, error))?;
    for path in [layout.preparation_path(), layout.rollback_path()] {
        match open_journal_regular(access, path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) | Err(_) => {
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::InvalidJournal {
                        message:
                            "transaction evidence changed while orphaned attempts were cleaned"
                                .to_owned(),
                    },
                ));
            }
        }
    }
    match open_journal_directory(access, layout.directory()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) | Err(_) => {
            return Err(blocked(
                locator,
                RecoveryBlockedReason::InvalidJournal {
                    message: "transaction evidence changed while orphaned attempts were cleaned"
                        .to_owned(),
                },
            ));
        }
    }
    Ok(RecoveryOutcome::NoTransaction(locator.clone()))
}

fn map_premanifest_cleanup_error(
    locator: &RecoveryLocator,
    error: PremanifestCleanupError,
) -> RecoveryError {
    match error {
        PremanifestCleanupError::Budget(source) => RecoveryError::Budget {
            locator: Box::new(locator.clone()),
            source,
        },
        PremanifestCleanupError::Blocked(reason) => blocked(locator, reason),
        PremanifestCleanupError::Preparation(error) => map_journal_open_error(locator, error),
        PremanifestCleanupError::Io(error) => blocked(
            locator,
            RecoveryBlockedReason::Io {
                message: error.to_string(),
            },
        ),
    }
}

#[derive(Debug, Error)]
pub(super) enum PremanifestCleanupError {
    #[error("the durable preparation record could not be opened: {0}")]
    Preparation(#[source] JournalError),
    #[error("premanifest cleanup exceeded its caller-owned budget: {0}")]
    Budget(#[source] BudgetError),
    #[error("premanifest cleanup was blocked by filesystem evidence: {0}")]
    Blocked(#[source] RecoveryBlockedReason),
    #[error("premanifest cleanup I/O failed: {0}")]
    Io(#[source] io::Error),
}

pub(super) fn cleanup_prepared_transaction(
    layout: &JournalLayout,
    access: &JournalAccess<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<(), PremanifestCleanupError> {
    let preparation = JournalPreparation::open_in_access(layout, access, budget)
        .map_err(PremanifestCleanupError::Preparation)?;
    let plan =
        observe_premanifest_cleanup(layout, access, preparation, budget).map_err(|error| {
            match error {
                ObservationError::Budget(error) => PremanifestCleanupError::Budget(error),
                ObservationError::Blocked(error) => PremanifestCleanupError::Blocked(error),
            }
        })?;
    execute_premanifest_cleanup(layout, access, &plan, PreparationCleanup::Remove)
        .map_err(PremanifestCleanupError::Io)
}

/// Removes freshly written premanifest evidence after the main caller budget
/// has already been exhausted.
///
/// This ledger remains finite. It is intentionally independent from the
/// failed operation so cleanup cannot mask its typed budget error and force a
/// caller to recover a transaction that never published a canonical manifest.
pub(super) fn cleanup_prepared_transaction_after_budget_exhaustion(
    layout: &JournalLayout,
    access: &JournalAccess<'_>,
) -> Result<(), PremanifestCleanupError> {
    let mut cleanup_budget = AssetLoadBudget::default();
    cleanup_prepared_transaction(layout, access, &mut cleanup_budget)
}

#[derive(Debug)]
struct OrphanedPreparationAttempt {
    identity: FileIdentity,
}

pub(super) fn cleanup_orphaned_preparation_attempts(
    layout: &JournalLayout,
    access: &JournalAccess<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<(), PremanifestCleanupError> {
    let attempt = observe_orphaned_preparation_attempt(layout, access, budget).map_err(
        |error| match error {
            ObservationError::Budget(error) => PremanifestCleanupError::Budget(error),
            ObservationError::Blocked(error) => PremanifestCleanupError::Blocked(error),
        },
    )?;
    let Some(attempt) = attempt else {
        return Ok(());
    };
    remove_journal_regular(
        access,
        layout.preparation_temporary_path(),
        &attempt.identity,
    )
    .map_err(PremanifestCleanupError::Io)?;
    sync_journal_access(access).map_err(PremanifestCleanupError::Io)
}

fn observe_orphaned_preparation_attempt(
    layout: &JournalLayout,
    access: &JournalAccess<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<Option<OrphanedPreparationAttempt>, ObservationError> {
    let path = layout.preparation_temporary_path();
    let file = match open_journal_regular(access, path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unexpected_premanifest("preparation-attempt-path").into()),
    };
    budget.consume_entries(1)?;
    let identity = opened_file_identity(&file).map_err(io_reason)?;
    Ok(Some(OrphanedPreparationAttempt { identity }))
}

#[derive(Debug)]
struct PremanifestCleanupFile {
    path: PathBuf,
    identity: FileIdentity,
    parent: PremanifestParentDirectory,
    parent_identity: DirectoryIdentity,
}

#[derive(Debug)]
struct PremanifestCleanupDirectory {
    kind: PremanifestPrivateDirectory,
    identity: DirectoryIdentity,
}

#[derive(Debug)]
struct PremanifestCleanupPlan {
    preparation: OpenedJournalPreparation,
    files: Vec<PremanifestCleanupFile>,
    directories: Vec<PremanifestCleanupDirectory>,
    transaction: Option<DirectoryIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PremanifestPrivateDirectory {
    Events,
    Stage,
    Backup,
    Baseline,
}

impl PremanifestPrivateDirectory {
    const ALL: [Self; 4] = [Self::Events, Self::Stage, Self::Backup, Self::Baseline];

    const fn index(self) -> usize {
        match self {
            Self::Events => 0,
            Self::Stage => 1,
            Self::Backup => 2,
            Self::Baseline => 3,
        }
    }

    fn from_entry_name(name: &str) -> Option<Self> {
        match name {
            EVENTS_DIRECTORY => Some(Self::Events),
            STAGE_DIRECTORY => Some(Self::Stage),
            BACKUP_DIRECTORY => Some(Self::Backup),
            BASELINE_DIRECTORY => Some(Self::Baseline),
            _ => None,
        }
    }

    fn path(self, layout: &JournalLayout) -> &Path {
        match self {
            Self::Events => layout.events_directory(),
            Self::Stage => layout.stage_directory(),
            Self::Backup => layout.backup_directory(),
            Self::Baseline => layout.baseline_directory(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PremanifestParentDirectory {
    Transaction,
    Stage,
    Baseline,
}

fn observe_premanifest_cleanup(
    layout: &JournalLayout,
    access: &JournalAccess<'_>,
    preparation: OpenedJournalPreparation,
    budget: &mut AssetLoadBudget,
) -> Result<PremanifestCleanupPlan, ObservationError> {
    ensure_final_manifest_absent(access, layout, None)?;
    let file_capacity = preparation
        .document()
        .outputs()
        .len()
        .checked_add(preparation.document().baseline().sources().len())
        .and_then(|count| count.checked_add(1))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "premanifest cleanup files",
        })?;
    let mut files = recovery_vec(file_capacity, "premanifest cleanup files", budget)?;
    let mut directories = recovery_vec(4, "premanifest cleanup directories", budget)?;
    let transaction = match open_journal_directory(access, layout.directory()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            preparation
                .revalidate_in_access(layout, access, budget)
                .map_err(preparation_observation_error)?;
            ensure_final_manifest_absent(access, layout, None)?;
            return Ok(PremanifestCleanupPlan {
                preparation,
                files,
                directories,
                transaction: None,
            });
        }
        Ok(transaction) => transaction,
        Err(_) => return Err(unexpected_premanifest("transaction-directory").into()),
    };
    let transaction_identity = journal_directory_identity(&transaction).map_err(io_reason)?;
    let mut present = [false; 4];
    let mut root_entries = 0_usize;
    let mut scratch = [0_u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES];
    visit_premanifest_directory(&transaction, budget, |budget, entry| {
        let Some(name_text) = premanifest_entry_name(entry, &mut scratch)? else {
            return Ok(());
        };
        root_entries = root_entries
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "premanifest transaction entries",
            })?;
        if root_entries > 5 {
            return Err(unexpected_premanifest("transaction-entry-count").into());
        }
        budget.consume_entries(1)?;
        if let Some(kind) = PremanifestPrivateDirectory::from_entry_name(name_text) {
            let index = kind.index();
            if present[index] {
                return Err(unexpected_premanifest(name_text).into());
            }
            present[index] = true;
            return Ok(());
        }
        if name_text == MANIFEST_TEMPORARY_FILE {
            let path = recovery_join_component(
                layout.directory(),
                OsStr::new(name_text),
                "premanifest manifest temporary path",
                budget,
            )?;
            observe_premanifest_file(
                path,
                &transaction,
                &transaction_identity,
                PremanifestParentDirectory::Transaction,
                &mut files,
            )?;
            return Ok(());
        }
        Err(unexpected_premanifest(name_text).into())
    })?;

    for kind in PremanifestPrivateDirectory::ALL {
        if !present[kind.index()] {
            continue;
        }
        let directory = open_journal_directory_in_directory(&transaction, kind.path(layout))
            .map_err(|_| unexpected_premanifest("private-directory"))?;
        let identity = journal_directory_identity(&directory)
            .map_err(|_| unexpected_premanifest("private-directory"))?;
        match kind {
            PremanifestPrivateDirectory::Events | PremanifestPrivateDirectory::Backup => {
                observe_empty_premanifest_directory(&directory, budget)?;
            }
            PremanifestPrivateDirectory::Stage => observe_stage_directory(
                layout,
                &directory,
                &identity,
                preparation.document(),
                &mut files,
                budget,
            )?,
            PremanifestPrivateDirectory::Baseline => observe_baseline_directory(
                layout,
                &directory,
                &identity,
                preparation.document(),
                &mut files,
                budget,
            )?,
        }
        directories.push(PremanifestCleanupDirectory { kind, identity });
    }
    preparation
        .revalidate_in_access(layout, access, budget)
        .map_err(preparation_observation_error)?;
    ensure_final_manifest_absent(access, layout, Some(&transaction_identity))?;
    Ok(PremanifestCleanupPlan {
        preparation,
        files,
        directories,
        transaction: Some(transaction_identity),
    })
}

fn observe_empty_premanifest_directory(
    directory: &JournalDirectory,
    budget: &mut AssetLoadBudget,
) -> Result<(), ObservationError> {
    let mut scratch = [0_u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES];
    visit_premanifest_directory(directory, budget, |budget, entry| {
        let Some(_) = premanifest_entry_name(entry, &mut scratch)? else {
            return Ok(());
        };
        budget.consume_entries(1)?;
        Err(unexpected_premanifest("non-empty-private-directory").into())
    })
}

fn observe_stage_directory(
    layout: &JournalLayout,
    directory: &JournalDirectory,
    parent: &DirectoryIdentity,
    preparation: &JournalPreparation,
    files: &mut Vec<PremanifestCleanupFile>,
    budget: &mut AssetLoadBudget,
) -> Result<(), ObservationError> {
    let mut count = 0_usize;
    let mut scratch = [0_u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES];
    visit_premanifest_directory(directory, budget, |budget, entry| {
        let Some(name_text) = premanifest_entry_name(entry, &mut scratch)? else {
            return Ok(());
        };
        count = count
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "premanifest stage entries",
            })?;
        if count > preparation.outputs().len() {
            return Err(unexpected_premanifest("stage-entry-count").into());
        }
        budget.consume_entries(1)?;
        let ordinal = parse_premanifest_ordinal(name_text, ".stage")
            .filter(|ordinal| *ordinal < preparation.outputs().len())
            .ok_or_else(|| unexpected_premanifest(name_text))?;
        if preparation.outputs()[ordinal].ordinal()
            != u32::try_from(ordinal)
                .map_err(|_| unexpected_premanifest("stage-ordinal-overflow"))?
        {
            return Err(unexpected_premanifest(name_text).into());
        }
        let entry_path = recovery_join_component(
            layout.stage_directory(),
            OsStr::new(name_text),
            "premanifest staged file path",
            budget,
        )?;
        observe_premanifest_file(
            entry_path,
            directory,
            parent,
            PremanifestParentDirectory::Stage,
            files,
        )?;
        Ok(())
    })
}

fn observe_baseline_directory(
    layout: &JournalLayout,
    directory: &JournalDirectory,
    parent: &DirectoryIdentity,
    preparation: &JournalPreparation,
    files: &mut Vec<PremanifestCleanupFile>,
    budget: &mut AssetLoadBudget,
) -> Result<(), ObservationError> {
    let sources = preparation.baseline().sources();
    let mut count = 0_usize;
    let mut scratch = [0_u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES];
    visit_premanifest_directory(directory, budget, |budget, entry| {
        let Some(name_text) = premanifest_entry_name(entry, &mut scratch)? else {
            return Ok(());
        };
        count = count
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "premanifest baseline entries",
            })?;
        if count > sources.len() {
            return Err(unexpected_premanifest("baseline-entry-count").into());
        }
        budget.consume_entries(1)?;
        let ordinal = parse_premanifest_ordinal(name_text, ".image")
            .filter(|ordinal| *ordinal < sources.len())
            .ok_or_else(|| unexpected_premanifest(name_text))?;
        let declared = matches!(
            sources[ordinal].image(),
            JournalBaselineImage::Blob { path, .. }
                if matches_ordinal_journal_path(path, "baseline/", ordinal, ".image")
        );
        if !declared {
            return Err(unexpected_premanifest(name_text).into());
        }
        let entry_path = recovery_join_component(
            layout.baseline_directory(),
            OsStr::new(name_text),
            "premanifest baseline file path",
            budget,
        )?;
        observe_premanifest_file(
            entry_path,
            directory,
            parent,
            PremanifestParentDirectory::Baseline,
            files,
        )?;
        Ok(())
    })
}

fn visit_premanifest_directory(
    directory: &JournalDirectory,
    budget: &mut AssetLoadBudget,
    visitor: impl FnMut(&mut AssetLoadBudget, DirectoryEntryName<'_>) -> Result<(), ObservationError>,
) -> Result<(), ObservationError> {
    budget.consume_bytes(DIRECTORY_VISIT_SETUP_BYTES)?;
    match visit_journal_directory_entries(
        directory,
        budget,
        |budget| {
            budget.consume_bytes(DIRECTORY_VISIT_ENTRY_BYTES)?;
            Ok(())
        },
        visitor,
    ) {
        Ok(()) => Ok(()),
        Err(DirectoryVisitError::Visitor(error)) => Err(error),
        Err(DirectoryVisitError::Io(error)) => Err(io_reason(error).into()),
    }
}

fn premanifest_entry_name<'a>(
    entry: DirectoryEntryName<'_>,
    scratch: &'a mut [u8; MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES],
) -> Result<Option<&'a str>, ObservationError> {
    let name = ascii_directory_entry_name(entry, scratch)
        .ok_or_else(|| unexpected_premanifest("non-utf8-private-directory-entry"))?;
    if matches!(name, "." | "..") {
        Ok(None)
    } else {
        Ok(Some(name))
    }
}

fn observe_premanifest_file(
    path: PathBuf,
    parent: &JournalDirectory,
    parent_identity: &DirectoryIdentity,
    parent_kind: PremanifestParentDirectory,
    files: &mut Vec<PremanifestCleanupFile>,
) -> Result<(), ObservationError> {
    let file = open_journal_regular_in_directory(parent, &path).map_err(io_reason)?;
    let identity = opened_file_identity(&file).map_err(io_reason)?;
    files.push(PremanifestCleanupFile {
        path,
        identity,
        parent: parent_kind,
        parent_identity: parent_identity.clone(),
    });
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparationCleanup {
    Remove,
    RetainRollbackReceipt,
}

fn execute_premanifest_cleanup(
    layout: &JournalLayout,
    access: &JournalAccess<'_>,
    plan: &PremanifestCleanupPlan,
    cleanup: PreparationCleanup,
) -> io::Result<()> {
    if let Some(transaction_identity) = &plan.transaction {
        ensure_final_manifest_absent_io(access, layout, Some(transaction_identity))?;
        for file in &plan.files {
            ensure_final_manifest_absent_io(access, layout, Some(transaction_identity))?;
            let parent = open_premanifest_cleanup_parent(
                access,
                layout,
                transaction_identity,
                file.parent,
                &file.parent_identity,
            )?;
            remove_journal_regular_in_directory(&parent, &file.path, &file.identity)?;
        }
        for directory in plan.directories.iter().rev() {
            ensure_final_manifest_absent_io(access, layout, Some(transaction_identity))?;
            let transaction = open_premanifest_transaction(access, layout, transaction_identity)?;
            remove_journal_directory_in_directory(
                &transaction,
                directory.kind.path(layout),
                &directory.identity,
            )?;
        }
        ensure_final_manifest_absent_io(access, layout, Some(transaction_identity))?;
        remove_journal_directory(access, layout.directory(), transaction_identity)?;
    } else {
        ensure_final_manifest_absent_io(access, layout, None)?;
    }
    match cleanup {
        PreparationCleanup::Remove => {
            remove_journal_regular(
                access,
                layout.preparation_path(),
                plan.preparation.identity(),
            )?;
            sync_journal_access(access)
        }
        PreparationCleanup::RetainRollbackReceipt => {
            capture_journal_regular(
                access,
                layout.preparation_path(),
                layout.rollback_path(),
                plan.preparation.identity(),
            )?;
            #[cfg(test)]
            super::test_crash_failpoint("premanifest_rollback_captured");
            remove_journal_regular(
                access,
                layout.preparation_path(),
                plan.preparation.identity(),
            )?;
            sync_journal_access(access)?;
            #[cfg(test)]
            super::test_crash_failpoint("premanifest_rollback_recorded");
            Ok(())
        }
    }
}

fn open_premanifest_transaction(
    access: &JournalAccess<'_>,
    layout: &JournalLayout,
    expected: &DirectoryIdentity,
) -> io::Result<JournalDirectory> {
    let transaction = open_journal_directory(access, layout.directory())?;
    ensure_premanifest_directory_identity(&transaction, expected)?;
    Ok(transaction)
}

fn open_premanifest_cleanup_parent(
    access: &JournalAccess<'_>,
    layout: &JournalLayout,
    transaction_identity: &DirectoryIdentity,
    parent: PremanifestParentDirectory,
    expected_parent: &DirectoryIdentity,
) -> io::Result<JournalDirectory> {
    let transaction = open_premanifest_transaction(access, layout, transaction_identity)?;
    match parent {
        PremanifestParentDirectory::Transaction => {
            if expected_parent != transaction_identity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "premanifest cleanup plan has an invalid transaction parent identity",
                ));
            }
            Ok(transaction)
        }
        PremanifestParentDirectory::Stage | PremanifestParentDirectory::Baseline => {
            let path = match parent {
                PremanifestParentDirectory::Stage => layout.stage_directory(),
                PremanifestParentDirectory::Baseline => layout.baseline_directory(),
                PremanifestParentDirectory::Transaction => unreachable!("handled above"),
            };
            let directory = open_journal_directory_in_directory(&transaction, path)?;
            ensure_premanifest_directory_identity(&directory, expected_parent)?;
            Ok(directory)
        }
    }
}

fn ensure_premanifest_directory_identity(
    directory: &JournalDirectory,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    if journal_directory_identity(directory)? != *expected {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "premanifest journal directory no longer matches its captured identity",
        ));
    }
    Ok(())
}

fn parse_premanifest_ordinal(name: &str, suffix: &str) -> Option<usize> {
    let ordinal = name.strip_suffix(suffix)?;
    if ordinal.len() != 8 || !ordinal.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    ordinal.parse().ok()
}

fn ensure_final_manifest_absent(
    access: &JournalAccess<'_>,
    layout: &JournalLayout,
    expected_transaction: Option<&DirectoryIdentity>,
) -> Result<(), ObservationError> {
    ensure_final_manifest_absent_io(access, layout, expected_transaction).map_err(io_reason)?;
    Ok(())
}

fn ensure_final_manifest_absent_io(
    access: &JournalAccess<'_>,
    layout: &JournalLayout,
    expected_transaction: Option<&DirectoryIdentity>,
) -> io::Result<()> {
    let transaction = match open_journal_directory(access, layout.directory()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return if expected_transaction.is_some() {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "premanifest transaction disappeared during recovery",
                ))
            } else {
                Ok(())
            };
        }
        Ok(transaction) => transaction,
        Err(error) => return Err(error),
    };
    if let Some(expected) = expected_transaction {
        ensure_premanifest_directory_identity(&transaction, expected)?;
    }
    match open_journal_regular_in_directory(&transaction, layout.manifest_path()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "final manifest appeared during premanifest recovery",
        )),
        Err(error) => Err(error),
    }
}

fn unexpected_premanifest(artifact: impl Into<String>) -> RecoveryBlockedReason {
    RecoveryBlockedReason::UnexpectedEvidence {
        artifact: artifact.into(),
    }
}

fn io_reason(error: io::Error) -> RecoveryBlockedReason {
    RecoveryBlockedReason::Io {
        message: error.to_string(),
    }
}

fn preparation_observation_error(error: JournalError) -> ObservationError {
    match error {
        JournalError::Budget(error) => ObservationError::Budget(error),
        error => ObservationError::Blocked(RecoveryBlockedReason::InvalidJournal {
            message: error.to_string(),
        }),
    }
}

fn recover_finalized_journal(
    workspace: Option<&mut AssetWorkspace>,
    journal: &Journal,
    locator: &RecoveryLocator,
    intent: RecoveryIntent,
    report: CommitReport,
    events: &EventFacts,
    baseline: BaselineObservation,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryOutcome, RecoveryError> {
    if events.blocked_reason.is_some() {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::InvalidEventSequence {
                message: "a finalized receipt retains a prior recovery-blocked event".to_owned(),
            },
        ));
    }
    if events.abandoned {
        if events.published
            || events.baseline_installed
            || events.direction != Some(RecoveryDirection::Rollback)
        {
            return Err(blocked(
                locator,
                RecoveryBlockedReason::InvalidEventSequence {
                    message: "finalized rollback receipt has incompatible events".to_owned(),
                },
            ));
        }
        // A rollback receipt is historical evidence. Its former target bytes
        // may have been superseded by a later publication, so terminal
        // redelivery must never inspect or restore them.
        return Ok(historical_rollback_receipt(&report));
    }
    if !events.published
        || !events.baseline_installed
        || events.artifacts.iter().any(|artifact| !artifact.promoted)
    {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::InvalidEventSequence {
                message:
                    "finalized publication does not contain a complete canonical event sequence"
                        .to_owned(),
            },
        ));
    }
    if intent == RecoveryIntent::Abandon {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::InvalidEventSequence {
                message: "a finalized publication cannot be abandoned".to_owned(),
            },
        ));
    }

    let Some(workspace) = workspace else {
        // Detached recovery only validates immutable journal evidence. It
        // intentionally does not compare current targets with a historical
        // digest because a later legitimate transaction may have superseded
        // every target since this receipt was finalized.
        return Ok(historical_commit_receipt(report));
    };
    match baseline {
        BaselineObservation::Committed => match observe_execution(journal, budget) {
            Ok((_, artifacts)) if artifacts.iter().all(|artifact| artifact.is_published()) => {
                Ok(commit_outcome(report, true))
            }
            Ok(_) | Err(ObservationError::Blocked(_)) => Ok(historical_commit_receipt(report)),
            Err(ObservationError::Budget(source)) => Err(RecoveryError::Budget {
                locator: Box::new(locator.clone()),
                source,
            }),
        },
        BaselineObservation::Base => {
            // Installing a baseline changes in-memory state, so it remains a
            // stronger operation than receipt redelivery. Verify the current
            // publication image only in this branch before rebuilding it.
            let (_, artifacts) = observe_execution(journal, budget)
                .map_err(|error| map_observation_error(locator, error))?;
            if artifacts.iter().any(|artifact| !artifact.is_published()) {
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::UnexpectedEvidence {
                        artifact: "finalized-publication".to_owned(),
                    },
                ));
            }
            let rebuilt =
                prebuild_recovery_baseline(workspace, journal, &artifacts, locator, budget)?;
            if !workspace.install_state_if_current(&rebuilt.expected, rebuilt.next) {
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::BaselineUnavailable {
                        expected: report.committed_revision(),
                        actual: workspace.revision(),
                    },
                ));
            }
            Ok(commit_outcome(report, true))
        }
        BaselineObservation::Other | BaselineObservation::Detached => {
            // The same workspace can legitimately have advanced through a
            // successor transaction. Redeliver the immutable receipt without
            // replacing its newer state.
            Ok(historical_commit_receipt(report))
        }
    }
}

fn recover_open_journal(
    mut workspace: Option<&mut AssetWorkspace>,
    journal: &mut Journal,
    locator: &RecoveryLocator,
    intent: RecoveryIntent,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryOutcome, RecoveryError> {
    let workspace_attached = workspace.is_some();
    let report = journal
        .manifest()
        .report(locator.root(), locator.root_identity(), budget)
        .map_err(|error| map_journal_mutation_error(locator, error))?;
    if let Some(workspace) = workspace.as_deref()
        && report.workspace_id() != workspace.workspace_id()
    {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::WorkspaceMismatch {
                expected: report.workspace_id(),
                actual: workspace.workspace_id(),
            },
        ));
    }

    let events = EventFacts::from_journal(journal, budget)
        .map_err(|error| map_observation_error(locator, error))?;
    let baseline = match workspace.as_deref() {
        Some(workspace) if workspace.revision() == report.committed_revision() => {
            BaselineObservation::Committed
        }
        Some(workspace) if workspace.revision() == report.base_revision() => {
            BaselineObservation::Base
        }
        Some(_) => BaselineObservation::Other,
        None => BaselineObservation::Detached,
    };
    if events.finalized {
        validate_manifest_paths(journal, budget)
            .map_err(|error| map_observation_error(locator, error))?;
        return recover_finalized_journal(
            workspace.as_deref_mut(),
            journal,
            locator,
            intent,
            report,
            &events,
            baseline,
            budget,
        );
    }
    let (execution, artifacts) = observe_execution(journal, budget)
        .map_err(|error| map_observation_error(locator, error))?;
    let mut observation = RecoveryObservation {
        events,
        artifacts,
        baseline,
    };
    if workspace.is_some()
        && (!observation.events.published
            || !observation
                .artifacts
                .iter()
                .all(|artifact| artifact.is_published()))
    {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::FilesystemRecoveryRequired,
        ));
    }
    let plan = match intent {
        RecoveryIntent::Resume => decide_recovery(&observation),
        RecoveryIntent::Abandon => decide_abandon(&observation),
    };

    match plan.disposition {
        RecoveryDisposition::Blocked => {
            let reason =
                plan.blocked
                    .unwrap_or_else(|| RecoveryBlockedReason::InvalidEventSequence {
                        message: "recovery was blocked without a reason".to_owned(),
                    });
            if matches!(reason, RecoveryBlockedReason::InvalidEventSequence { .. })
                && (observation.events.published
                    || observation.events.baseline_installed
                    || observation.events.finalized)
                && workspace
                    .as_deref()
                    .is_some_and(|workspace| workspace.revision() != report.committed_revision())
            {
                let actual = workspace
                    .as_deref()
                    .expect("baseline mismatch has an attached workspace")
                    .revision();
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::BaselineUnavailable {
                        expected: report.committed_revision(),
                        actual,
                    },
                ));
            }
            if observation.events.direction == Some(RecoveryDirection::Forward)
                && !observation.events.published
                && !observation.events.baseline_installed
                && !observation.events.finalized
                && observation
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.has_repairable_owned_corruption())
            {
                let repairs =
                    plan_owned_corruption_repairs(&execution, &observation.artifacts, budget)
                        .map_err(|error| map_observation_error(locator, error))?;
                execute_owned_corruption_repairs(journal, &execution, &repairs)
                    .map_err(|error| map_execution_error(locator, error))?;
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::UnexpectedEvidence {
                        artifact: "transaction-owned-corruption".to_owned(),
                    },
                ));
            }
            if intent == RecoveryIntent::Abandon {
                return Err(blocked(locator, reason));
            }
            block_and_record(journal, locator, reason, budget)
        }
        RecoveryDisposition::Forward => {
            if observation.events.finalized
                && observation.baseline == BaselineObservation::Committed
            {
                return Ok(commit_outcome(report, workspace_attached));
            }
            let already_published = observation
                .artifacts
                .iter()
                .all(|artifact| artifact.is_published());
            let finalize_workspace = workspace.is_some();
            let prebuilt_baseline = if matches!(
                observation.baseline,
                BaselineObservation::Base | BaselineObservation::Other
            ) {
                Some(prebuild_recovery_baseline(
                    workspace
                        .as_deref()
                        .expect("an attached baseline observation has a workspace"),
                    journal,
                    &observation.artifacts,
                    locator,
                    budget,
                )?)
            } else {
                None
            };
            let event_keys =
                recovery_event_keys(&observation, plan.disposition, finalize_workspace, budget)
                    .map_err(|error| map_observation_error(locator, error))?;
            let mut event_plan = journal
                .plan_events(&event_keys, budget)
                .map_err(|error| map_journal_mutation_error(locator, error))?;
            if !already_published {
                precharge_execution_verification(
                    journal,
                    &observation.artifacts,
                    RecoveryDisposition::Forward,
                    budget,
                )
                .map_err(|error| map_observation_error(locator, error))?;
            }
            persist_direction(
                journal,
                &mut observation.events,
                RecoveryDirection::Forward,
                &mut event_plan,
            )
            .map_err(|error| map_journal_mutation_error(locator, error))?;
            if already_published {
                finish_published_events(journal, &mut observation.events, &mut event_plan)
                    .map_err(|error| map_execution_error(locator, error))?;
            } else {
                roll_forward(
                    journal,
                    &mut observation.events,
                    &mut observation.artifacts,
                    &execution,
                    &mut event_plan,
                    budget,
                )
                .map_err(|error| map_execution_error(locator, error))?;
            }

            if let Some(workspace) = workspace.as_deref_mut() {
                if workspace.revision() != report.committed_revision() {
                    let baseline = prebuilt_baseline
                        .expect("non-committed recovery prebuilds its workspace baseline");
                    if !workspace.install_state_if_current(&baseline.expected, baseline.next) {
                        return Err(blocked(
                            locator,
                            RecoveryBlockedReason::BaselineUnavailable {
                                expected: report.committed_revision(),
                                actual: workspace.revision(),
                            },
                        ));
                    }
                    observation.baseline = BaselineObservation::Committed;
                }
                append_once(
                    journal,
                    &mut observation.events.baseline_installed,
                    JournalEventKey::BaselineInstalled,
                    &mut event_plan,
                )
                .and_then(|_| {
                    append_once(
                        journal,
                        &mut observation.events.finalized,
                        JournalEventKey::Finalized,
                        &mut event_plan,
                    )
                })
                .map_err(|error| map_journal_mutation_error(locator, error))?;
            }
            event_plan
                .finish()
                .map_err(|error| map_journal_mutation_error(locator, error))?;
            Ok(commit_outcome(report, workspace_attached))
        }
        RecoveryDisposition::Rollback => {
            if observation.events.finalized && observation.events.abandoned {
                return Ok(rollback_outcome(&report));
            }
            if !matches!(
                observation.baseline,
                BaselineObservation::Base | BaselineObservation::Detached
            ) {
                let actual = workspace
                    .as_deref()
                    .expect("attached rollback has a workspace")
                    .revision();
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::BaselineUnavailable {
                        expected: report.base_revision(),
                        actual,
                    },
                ));
            }
            let event_keys = recovery_event_keys(&observation, plan.disposition, true, budget)
                .map_err(|error| map_observation_error(locator, error))?;
            let mut event_plan = journal
                .plan_events(&event_keys, budget)
                .map_err(|error| map_journal_mutation_error(locator, error))?;
            precharge_execution_verification(
                journal,
                &observation.artifacts,
                RecoveryDisposition::Rollback,
                budget,
            )
            .map_err(|error| map_observation_error(locator, error))?;
            persist_direction(
                journal,
                &mut observation.events,
                RecoveryDirection::Rollback,
                &mut event_plan,
            )
            .map_err(|error| map_journal_mutation_error(locator, error))?;
            roll_back(journal, &mut observation.artifacts, &execution)
                .map_err(|error| map_execution_error(locator, error))?;
            append_once(
                journal,
                &mut observation.events.abandoned,
                JournalEventKey::Abandoned,
                &mut event_plan,
            )
            .and_then(|_| {
                append_once(
                    journal,
                    &mut observation.events.finalized,
                    JournalEventKey::Finalized,
                    &mut event_plan,
                )
            })
            .map_err(|error| map_journal_mutation_error(locator, error))?;
            event_plan
                .finish()
                .map_err(|error| map_journal_mutation_error(locator, error))?;
            Ok(rollback_outcome(&report))
        }
    }
}

fn rollback_outcome(report: &CommitReport) -> RecoveryOutcome {
    RecoveryOutcome::RolledBack(RollbackReceipt::new(
        report.workspace_id(),
        report.base_revision(),
        report.recovery().clone(),
    ))
}

fn commit_outcome(report: CommitReport, workspace_attached: bool) -> RecoveryOutcome {
    if workspace_attached {
        RecoveryOutcome::Finalized(Box::new(report))
    } else {
        RecoveryOutcome::FilesystemRecovered(Box::new(report))
    }
}

fn historical_commit_receipt(report: CommitReport) -> RecoveryOutcome {
    RecoveryOutcome::HistoricalCommitReceipt(Box::new(report))
}

fn historical_rollback_receipt(report: &CommitReport) -> RecoveryOutcome {
    RecoveryOutcome::HistoricalRollbackReceipt(RollbackReceipt::new(
        report.workspace_id(),
        report.base_revision(),
        report.recovery().clone(),
    ))
}

impl EventFacts {
    fn from_journal(
        journal: &Journal,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ObservationError> {
        let manifest = journal.manifest();
        let mut by_target = recovery_vec(
            manifest.artifacts().len(),
            "recovery event target index",
            budget,
        )?;
        by_target.extend(0..manifest.artifacts().len());
        by_target.sort_unstable_by(|left, right| {
            manifest.artifacts()[*left]
                .target()
                .cmp(manifest.artifacts()[*right].target())
        });
        for pair in by_target.windows(2) {
            if manifest.artifacts()[pair[0]].target() == manifest.artifacts()[pair[1]].target() {
                return Err(RecoveryBlockedReason::InvalidEventSequence {
                    message: "two artifacts use the same target".to_owned(),
                }
                .into());
            }
        }
        let mut artifacts = recovery_vec(
            manifest.artifacts().len(),
            "recovery artifact event facts",
            budget,
        )?;
        artifacts.resize_with(manifest.artifacts().len(), ArtifactEventFacts::default);
        let mut facts = Self {
            artifacts,
            ..Self::default()
        };

        for event in journal.events() {
            facts
                .apply(event, manifest.artifacts(), &by_target)
                .map_err(ObservationError::Blocked)?;
        }
        Ok(facts)
    }

    fn apply(
        &mut self,
        event: &JournalEvent,
        artifacts: &[JournalArtifact],
        by_target: &[usize],
    ) -> Result<(), RecoveryBlockedReason> {
        if self.finalized && !matches!(event.kind(), JournalEventKind::Marker { .. }) {
            return Err(invalid_event("a non-marker event follows Finalized"));
        }
        if self.direction == Some(RecoveryDirection::Rollback)
            && matches!(
                event.kind(),
                JournalEventKind::StagingVerified
                    | JournalEventKind::Journaled
                    | JournalEventKind::BackupIntent { .. }
                    | JournalEventKind::BackupCaptured { .. }
                    | JournalEventKind::PromotionIntent { .. }
                    | JournalEventKind::Promoted { .. }
                    | JournalEventKind::Published
                    | JournalEventKind::BaselineInstalled
            )
        {
            return Err(invalid_event(
                "a forward publication event follows a rollback decision",
            ));
        }
        if matches!(
            event.kind(),
            JournalEventKind::BackupIntent { .. }
                | JournalEventKind::BackupCaptured { .. }
                | JournalEventKind::PromotionIntent { .. }
                | JournalEventKind::Promoted { .. }
        ) && !self.journaled
        {
            return Err(invalid_event("an artifact event precedes Journaled"));
        }
        match event.kind() {
            JournalEventKind::StagingVerified => set_once(
                &mut self.staging_verified,
                "StagingVerified appears more than once",
            )?,
            JournalEventKind::Journaled => {
                if !self.staging_verified {
                    return Err(invalid_event("Journaled precedes StagingVerified"));
                }
                set_once(&mut self.journaled, "Journaled appears more than once")?;
            }
            JournalEventKind::BackupIntent { artifact } => {
                let index = event_artifact(artifacts, by_target, artifact)?;
                if artifacts[index].backup().is_none() {
                    return Err(invalid_event(
                        "backup intent names an artifact without a backup",
                    ));
                }
                set_once(
                    &mut self.artifacts[index].backup_intent,
                    "backup intent appears more than once",
                )?;
            }
            JournalEventKind::BackupCaptured { artifact } => {
                let index = event_artifact(artifacts, by_target, artifact)?;
                if !self.artifacts[index].backup_intent {
                    return Err(invalid_event("backup capture has no durable intent"));
                }
                set_once(
                    &mut self.artifacts[index].backup_captured,
                    "backup capture appears more than once",
                )?;
            }
            JournalEventKind::PromotionIntent { artifact } => {
                let index = event_artifact(artifacts, by_target, artifact)?;
                if artifacts[index].backup().is_some() && !self.artifacts[index].backup_captured {
                    return Err(invalid_event("promotion intent precedes backup capture"));
                }
                set_once(
                    &mut self.artifacts[index].promotion_intent,
                    "promotion intent appears more than once",
                )?;
            }
            JournalEventKind::Promoted { artifact } => {
                let index = event_artifact(artifacts, by_target, artifact)?;
                if !self.artifacts[index].promotion_intent {
                    return Err(invalid_event("promotion completion has no durable intent"));
                }
                set_once(
                    &mut self.artifacts[index].promoted,
                    "promotion completion appears more than once",
                )?;
            }
            JournalEventKind::Published => {
                if self.artifacts.iter().any(|artifact| !artifact.promoted) {
                    return Err(invalid_event("Published precedes an artifact promotion"));
                }
                set_once(&mut self.published, "Published appears more than once")?;
            }
            JournalEventKind::BaselineInstalled => {
                if !self.published || self.abandoned {
                    return Err(invalid_event(
                        "BaselineInstalled does not follow a published transaction",
                    ));
                }
                set_once(
                    &mut self.baseline_installed,
                    "BaselineInstalled appears more than once",
                )?;
            }
            JournalEventKind::Finalized => {
                if !self.baseline_installed && !self.abandoned {
                    return Err(invalid_event(
                        "Finalized has neither an installed baseline nor rollback",
                    ));
                }
                set_once(&mut self.finalized, "Finalized appears more than once")?;
            }
            JournalEventKind::RecoveryDecision { direction } => {
                if *direction == RecoveryDirection::Rollback
                    && (self.published || self.baseline_installed || self.abandoned)
                {
                    return Err(invalid_event(
                        "a rollback decision follows completed forward publication",
                    ));
                }
                if self.direction.replace(*direction).is_some() {
                    return Err(RecoveryBlockedReason::ConflictingDecision);
                }
            }
            JournalEventKind::Abandoned => {
                if self.direction != Some(RecoveryDirection::Rollback)
                    || self.published
                    || self.baseline_installed
                {
                    return Err(invalid_event("Abandoned has no valid rollback decision"));
                }
                set_once(&mut self.abandoned, "Abandoned appears more than once")?;
            }
            JournalEventKind::RecoveryBlocked { reason } => {
                if self.blocked_reason.replace(reason.clone()).is_some() {
                    return Err(invalid_event("RecoveryBlocked appears more than once"));
                }
            }
            JournalEventKind::Marker { .. } => {}
        }
        Ok(())
    }
}

fn event_artifact(
    artifacts: &[JournalArtifact],
    by_target: &[usize],
    artifact: &super::journal::JournalPath,
) -> Result<usize, RecoveryBlockedReason> {
    by_target
        .binary_search_by(|index| artifacts[*index].target().cmp(artifact))
        .map(|position| by_target[position])
        .map_err(|_| invalid_event("an event names an artifact outside the manifest"))
}

fn set_once(value: &mut bool, message: &'static str) -> Result<(), RecoveryBlockedReason> {
    if *value {
        return Err(invalid_event(message));
    }
    *value = true;
    Ok(())
}

fn invalid_event(message: impl Into<String>) -> RecoveryBlockedReason {
    RecoveryBlockedReason::InvalidEventSequence {
        message: message.into(),
    }
}

fn layout_from_locator(
    locator: &RecoveryLocator,
    budget: &mut AssetLoadBudget,
) -> Result<JournalLayout, RecoveryError> {
    let directory = locator.root();
    if !directory.is_absolute()
        || directory
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(blocked(
            locator,
            invalid_locator("the transaction path is not absolute and normalized"),
        ));
    }
    let version = directory
        .parent()
        .ok_or_else(|| blocked(locator, invalid_locator("the version directory is missing")))?;
    let recovery = version.parent().ok_or_else(|| {
        blocked(
            locator,
            invalid_locator("the recovery directory is missing"),
        )
    })?;
    let parent = recovery.parent().ok_or_else(|| {
        blocked(
            locator,
            invalid_locator("the destination parent is missing"),
        )
    })?;
    if version.file_name().and_then(|name| name.to_str()) != Some(RECOVERY_VERSION_DIRECTORY)
        || recovery.file_name().and_then(|name| name.to_str()) != Some(RECOVERY_DIRECTORY)
    {
        return Err(blocked(
            locator,
            invalid_locator("the recovery namespace version is unsupported"),
        ));
    }
    let current_root_identity = observe_directory_identity(parent)
        .map_err(|error| map_commit_guard_error(locator, error))?;
    if current_root_identity != *locator.root_identity() {
        return Err(blocked(
            locator,
            invalid_locator("publication root identity no longer matches the recovery locator"),
        ));
    }
    let layout = JournalLayout::new_budgeted(
        parent,
        locator.transaction(),
        locator.root_identity().clone(),
        budget,
    )
    .map_err(|error| map_layout_error(locator, error))?;
    if layout.directory() != directory {
        return Err(blocked(
            locator,
            invalid_locator("the transaction directory does not match its transaction digest"),
        ));
    }
    validate_directory(parent, "destination parent").map_err(|reason| blocked(locator, reason))?;
    if validate_optional_directory(recovery, "recovery directory")
        .map_err(|reason| blocked(locator, reason))?
    {
        validate_optional_directory(version, "recovery version directory")
            .map_err(|reason| blocked(locator, reason))?;
    }
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(blocked(
                locator,
                invalid_locator("transaction directory is not a non-symlink directory"),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(blocked(
                locator,
                RecoveryBlockedReason::Io {
                    message: format!("failed to inspect transaction directory: {error}"),
                },
            ));
        }
    }
    Ok(layout)
}

fn map_layout_error(locator: &RecoveryLocator, error: JournalError) -> RecoveryError {
    match error {
        JournalError::Budget(source) => RecoveryError::Budget {
            locator: Box::new(locator.clone()),
            source,
        },
        JournalError::Allocation { message, .. } => {
            blocked(locator, RecoveryBlockedReason::Io { message })
        }
        error => blocked(locator, invalid_locator(error.to_string())),
    }
}

fn invalid_locator(message: impl Into<String>) -> RecoveryBlockedReason {
    RecoveryBlockedReason::InvalidLocator {
        message: message.into(),
    }
}

fn validate_directory(path: &Path, label: &'static str) -> Result<(), RecoveryBlockedReason> {
    let metadata = fs::symlink_metadata(path).map_err(|error| RecoveryBlockedReason::Io {
        message: format!("failed to inspect {label}: {error}"),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_locator(format!(
            "{label} is not a non-symlink directory"
        )));
    }
    Ok(())
}

fn validate_optional_directory(
    path: &Path,
    label: &'static str,
) -> Result<bool, RecoveryBlockedReason> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            invalid_locator(format!("{label} is not a non-symlink directory")),
        ),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(RecoveryBlockedReason::Io {
            message: format!("failed to inspect {label}: {error}"),
        }),
    }
}

#[derive(Debug, Error)]
enum ObservationError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Blocked(#[from] RecoveryBlockedReason),
}

fn observe_execution(
    journal: &Journal,
    budget: &mut AssetLoadBudget,
) -> Result<(RecoveryExecutionPlan, Vec<ArtifactObservation>), ObservationError> {
    validate_manifest_paths(journal, budget)?;
    let execution = RecoveryExecutionPlan::build(journal, budget)?;
    let observations = observe_artifacts(journal, &execution, budget)?;
    Ok((execution, observations))
}

fn observe_artifacts(
    journal: &Journal,
    execution: &RecoveryExecutionPlan,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ArtifactObservation>, ObservationError> {
    if execution.artifacts.len() != journal.manifest().artifacts().len() {
        return Err(RecoveryBlockedReason::InvalidJournal {
            message: "recovery execution plan does not cover every artifact".to_owned(),
        }
        .into());
    }
    let mut observations = recovery_vec(
        journal.manifest().artifacts().len(),
        "recovery observations",
        budget,
    )?;
    for (artifact, paths) in journal
        .manifest()
        .artifacts()
        .iter()
        .zip(&execution.artifacts)
    {
        observations.push(observe_artifact(journal, artifact, paths, budget)?);
    }
    Ok(observations)
}

fn validate_manifest_paths(
    journal: &Journal,
    budget: &mut AssetLoadBudget,
) -> Result<(), ObservationError> {
    let mut paths = recovery_vec(
        journal.manifest().artifacts().len(),
        "recovery portable target index",
        budget,
    )?;
    for (ordinal, artifact) in journal.manifest().artifacts().iter().enumerate() {
        let key = slash_key(artifact.target().as_str(), budget).map_err(map_portable_path_error)?;
        if key == "/.unity-asset-recovery" || key.starts_with("/.unity-asset-recovery/") {
            return Err(RecoveryBlockedReason::UnsafePath {
                artifact: artifact.logical_name().to_owned(),
                role: "target",
            }
            .into());
        }
        if !matches_ordinal_journal_path(artifact.staging(), "stage/", ordinal, ".stage") {
            return Err(RecoveryBlockedReason::UnsafePath {
                artifact: artifact.logical_name().to_owned(),
                role: "staging",
            }
            .into());
        }
        if artifact.backup().is_some_and(|backup| {
            !matches_ordinal_journal_path(backup, "backup/", ordinal, ".backup")
        }) {
            return Err(RecoveryBlockedReason::UnsafePath {
                artifact: artifact.logical_name().to_owned(),
                role: "backup",
            }
            .into());
        }
        if artifact.old_digest().is_some() != artifact.backup().is_some()
            || artifact.old_digest().is_some() != artifact.old_identity().is_some()
        {
            return Err(RecoveryBlockedReason::InvalidJournal {
                message: format!(
                    "artifact {:?} disagrees about whether an old image exists",
                    artifact.logical_name()
                ),
            }
            .into());
        }

        paths.push((key, ordinal));
    }
    paths.sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    for pair in paths.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(RecoveryBlockedReason::InvalidJournal {
                message: "artifact targets collide under case-insensitive path rules".to_owned(),
            }
            .into());
        }
    }
    Ok(())
}

fn recovery_vec<T>(
    count: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ObservationError> {
    let entries = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    let requested = vec_allocation_bytes::<T>(count)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_entries(entries)?;
    budget.check_bytes(requested)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|error| RecoveryBlockedReason::Io {
            message: format!("failed to reserve {resource}: {error}"),
        })?;
    let actual = size_of::<T>()
        .checked_mul(values.capacity())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(actual)?;
    Ok(values)
}

impl RecoveryExecutionPlan {
    fn build(journal: &Journal, budget: &mut AssetLoadBudget) -> Result<Self, ObservationError> {
        let manifest = journal.manifest();
        let mut artifacts = recovery_vec(
            manifest.artifacts().len(),
            "recovery execution paths",
            budget,
        )?;
        for (index, artifact) in manifest.artifacts().iter().enumerate() {
            let ordinal =
                u32::try_from(index).map_err(|_| RecoveryBlockedReason::InvalidJournal {
                    message: "recovery artifact ordinal overflowed".to_owned(),
                })?;
            let old_digest = artifact.old_digest();
            let old_identity = artifact.old_identity().cloned();
            let backup = match (old_digest, old_identity.as_ref(), artifact.backup()) {
                (Some(_), Some(_), Some(backup)) => Some(recovery_join(
                    journal.layout().directory(),
                    backup,
                    "recovery execution backup path",
                    budget,
                )?),
                (None, None, None) => None,
                _ => {
                    return Err(RecoveryBlockedReason::InvalidJournal {
                        message: "recovery artifact old-image declaration is inconsistent"
                            .to_owned(),
                    }
                    .into());
                }
            };
            artifacts.push(RecoveryArtifactExecution {
                ordinal,
                target: recovery_join(
                    journal.layout().parent(),
                    artifact.target(),
                    "recovery execution target path",
                    budget,
                )?,
                staging: recovery_join(
                    journal.layout().directory(),
                    artifact.staging(),
                    "recovery execution staging path",
                    budget,
                )?,
                backup,
                target_parent_identity: artifact.destination_parent_identity().clone(),
                old_digest,
                old_identity,
                new_digest: artifact.new_digest(),
                new_identity: artifact.new_identity().clone(),
            });
        }
        Ok(Self { artifacts })
    }
}

fn map_portable_path_error(error: PortablePathError) -> ObservationError {
    match error {
        PortablePathError::Budget(error) => ObservationError::Budget(error),
        PortablePathError::UnsupportedEncoding => {
            ObservationError::Blocked(RecoveryBlockedReason::InvalidJournal {
                message: "journal target path has unsupported encoding".to_owned(),
            })
        }
        PortablePathError::Allocation { message, .. } => {
            ObservationError::Blocked(RecoveryBlockedReason::Io { message })
        }
    }
}

fn observe_artifact(
    journal: &Journal,
    artifact: &JournalArtifact,
    paths: &RecoveryArtifactExecution,
    budget: &mut AssetLoadBudget,
) -> Result<ArtifactObservation, ObservationError> {
    let layout = journal.layout();
    validate_ancestors(
        layout.parent(),
        &paths.target,
        artifact.logical_name(),
        "target",
    )?;
    let target_parent = paths
        .target
        .parent()
        .ok_or_else(|| RecoveryBlockedReason::UnsafePath {
            artifact: artifact.logical_name().to_owned(),
            role: "target parent",
        })?;
    let actual_parent =
        observe_directory_identity(target_parent).map_err(|error| RecoveryBlockedReason::Io {
            message: format!("failed to verify target parent identity: {error}"),
        })?;
    if actual_parent != paths.target_parent_identity {
        return Err(RecoveryBlockedReason::UnsafePath {
            artifact: artifact.logical_name().to_owned(),
            role: "target parent",
        }
        .into());
    }
    validate_ancestors(
        layout.directory(),
        &paths.staging,
        artifact.logical_name(),
        "staging",
    )?;
    if let Some(backup) = &paths.backup {
        validate_ancestors(
            layout.directory(),
            backup,
            artifact.logical_name(),
            "backup",
        )?;
    }

    Ok(ArtifactObservation {
        target: classify_target(&paths.target, artifact, budget)?,
        staging: classify_new(
            &paths.staging,
            artifact,
            journal.manifest().directories().stage(),
            budget,
        )?,
        backup: match (
            paths.backup.as_deref(),
            artifact.old_digest(),
            artifact.old_identity(),
        ) {
            (Some(path), Some(old), Some(identity)) => classify_old(
                path,
                old,
                identity,
                journal.manifest().directories().backup(),
                budget,
            )?,
            (None, None, None) => EntryEvidence::Missing,
            _ => EntryEvidence::Unexpected,
        },
        had_original: artifact.old_digest().is_some(),
    })
}

fn validate_ancestors(
    root: &Path,
    path: &Path,
    artifact: &str,
    role: &'static str,
) -> Result<(), RecoveryBlockedReason> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| RecoveryBlockedReason::UnsafePath {
            artifact: artifact.to_owned(),
            role,
        })?;
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(RecoveryBlockedReason::UnsafePath {
            artifact: artifact.to_owned(),
            role,
        });
    }

    let mut current = path
        .parent()
        .ok_or_else(|| RecoveryBlockedReason::UnsafePath {
            artifact: artifact.to_owned(),
            role,
        })?;
    while current != root {
        let metadata =
            fs::symlink_metadata(current).map_err(|error| RecoveryBlockedReason::Io {
                message: format!("failed to inspect an {role} ancestor: {error}"),
            })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(RecoveryBlockedReason::UnsafePath {
                artifact: artifact.to_owned(),
                role,
            });
        }
        current = current
            .parent()
            .ok_or_else(|| RecoveryBlockedReason::UnsafePath {
                artifact: artifact.to_owned(),
                role,
            })?;
    }
    Ok(())
}

fn classify_target(
    path: &Path,
    artifact: &JournalArtifact,
    budget: &mut AssetLoadBudget,
) -> Result<EntryEvidence, ObservationError> {
    match read_digest(path, artifact.destination_parent_identity(), budget)? {
        None => Ok(EntryEvidence::Missing),
        Some((digest, bytes, identity))
            if bytes != u64::MAX
                && bytes == artifact.bytes()
                && &identity == artifact.new_identity() =>
        {
            Ok(if digest == artifact.new_digest() {
                EntryEvidence::New
            } else {
                EntryEvidence::CorruptNew
            })
        }
        Some((digest, bytes, identity))
            if bytes != u64::MAX && artifact.old_identity() == Some(&identity) =>
        {
            Ok(if artifact.old_digest() == Some(digest) {
                EntryEvidence::Old
            } else {
                EntryEvidence::CorruptOld
            })
        }
        Some(_) => Ok(EntryEvidence::Unexpected),
    }
}

fn classify_new(
    path: &Path,
    artifact: &JournalArtifact,
    expected_parent: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<EntryEvidence, ObservationError> {
    match read_digest(path, expected_parent, budget)? {
        None => Ok(EntryEvidence::Missing),
        Some((digest, bytes, identity))
            if bytes != u64::MAX
                && bytes == artifact.bytes()
                && &identity == artifact.new_identity() =>
        {
            Ok(if digest == artifact.new_digest() {
                EntryEvidence::New
            } else {
                EntryEvidence::CorruptNew
            })
        }
        Some(_) => Ok(EntryEvidence::Unexpected),
    }
}

fn classify_old(
    path: &Path,
    old: DigestV1,
    expected_identity: &FileIdentity,
    expected_parent: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<EntryEvidence, ObservationError> {
    match read_digest(path, expected_parent, budget)? {
        None => Ok(EntryEvidence::Missing),
        Some((digest, bytes, identity)) if bytes != u64::MAX && &identity == expected_identity => {
            Ok(if digest == old {
                EntryEvidence::Old
            } else {
                EntryEvidence::CorruptOld
            })
        }
        Some(_) => Ok(EntryEvidence::Unexpected),
    }
}

fn read_digest(
    path: &Path,
    expected_parent: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<Option<(DigestV1, u64, FileIdentity)>, ObservationError> {
    let mut file = match open_readonly_regular_in_parent(path, expected_parent) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            return Ok(Some((
                DigestV1::hash_bytes(b"unsafe-entry"),
                u64::MAX,
                FileIdentity::invalid_sentinel(),
            )));
        }
        Err(error) => {
            return Err(RecoveryBlockedReason::Io {
                message: error.to_string(),
            }
            .into());
        }
    };
    let metadata = file.metadata().map_err(|error| RecoveryBlockedReason::Io {
        message: error.to_string(),
    })?;
    let length = metadata.len();
    let identity = opened_file_identity(&file).map_err(|error| RecoveryBlockedReason::Io {
        message: error.to_string(),
    })?;
    budget.consume_entries(1)?;
    budget.consume_bytes(length)?;
    let digest = match DigestV1::hash_reader(&mut file, length) {
        Ok(digest) => digest,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(Some((
                DigestV1::hash_bytes(b"unstable-entry"),
                u64::MAX,
                identity,
            )));
        }
        Err(error) => {
            return Err(RecoveryBlockedReason::Io {
                message: error.to_string(),
            }
            .into());
        }
    };
    Ok(Some((digest, length, identity)))
}

fn persist_direction(
    journal: &mut Journal,
    facts: &mut EventFacts,
    direction: RecoveryDirection,
    event_plan: &mut JournalEventPlan,
) -> Result<(), super::journal::JournalError> {
    match facts.direction {
        Some(existing) if existing == direction => Ok(()),
        Some(_) => Err(super::journal::JournalError::InvalidEvent(
            "recovery direction changed after it was persisted".to_owned(),
        )),
        None => {
            journal.append_planned(event_plan, JournalEventKey::RecoveryDecision(direction))?;
            facts.direction = Some(direction);
            Ok(())
        }
    }
}

fn precharge_execution_verification(
    journal: &Journal,
    observations: &[ArtifactObservation],
    disposition: RecoveryDisposition,
    budget: &mut AssetLoadBudget,
) -> Result<(), ObservationError> {
    if observations.len() != journal.manifest().artifacts().len() {
        return Err(RecoveryBlockedReason::InvalidJournal {
            message: "recovery execution observations are incomplete".to_owned(),
        }
        .into());
    }
    let mut entries = 0_u64;
    let mut bytes = 0_u64;
    for (artifact, observation) in journal.manifest().artifacts().iter().zip(observations) {
        let (old_reads, new_reads) = match (disposition, observation.had_original) {
            (RecoveryDisposition::Forward, true) => {
                match (observation.target, observation.staging, observation.backup) {
                    (EntryEvidence::Old, EntryEvidence::New, EntryEvidence::Missing) => (2, 2),
                    (EntryEvidence::Missing, EntryEvidence::New, EntryEvidence::Old) => (1, 2),
                    (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::Old) => (0, 1),
                    _ => return Err(invalid_event("forward verification evidence changed").into()),
                }
            }
            (RecoveryDisposition::Forward, false) => {
                match (observation.target, observation.staging, observation.backup) {
                    (EntryEvidence::Missing, EntryEvidence::New, EntryEvidence::Missing) => (0, 2),
                    (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::Missing) => (0, 1),
                    _ => return Err(invalid_event("forward verification evidence changed").into()),
                }
            }
            (RecoveryDisposition::Rollback, true) => {
                match (observation.target, observation.staging, observation.backup) {
                    (
                        EntryEvidence::Old,
                        EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                        EntryEvidence::Missing,
                    )
                    | (
                        EntryEvidence::Missing,
                        EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                        EntryEvidence::CorruptOld,
                    )
                    | (
                        EntryEvidence::New | EntryEvidence::CorruptNew,
                        EntryEvidence::Missing,
                        EntryEvidence::CorruptOld,
                    ) => (0, 0),
                    (
                        EntryEvidence::Missing,
                        EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                        EntryEvidence::Old,
                    )
                    | (
                        EntryEvidence::New | EntryEvidence::CorruptNew,
                        EntryEvidence::Missing,
                        EntryEvidence::Old,
                    ) => (1, 0),
                    _ => return Err(invalid_event("rollback verification evidence changed").into()),
                }
            }
            (RecoveryDisposition::Rollback, false) => {
                match (observation.target, observation.staging, observation.backup) {
                    (
                        EntryEvidence::Missing,
                        EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                        EntryEvidence::Missing,
                    )
                    | (
                        EntryEvidence::New | EntryEvidence::CorruptNew,
                        EntryEvidence::Missing,
                        EntryEvidence::Missing,
                    ) => (0, 0),
                    _ => return Err(invalid_event("rollback verification evidence changed").into()),
                }
            }
            (RecoveryDisposition::Blocked, _) => {
                return Err(invalid_event("blocked recovery cannot enter execution").into());
            }
        };
        if old_reads != 0 {
            add_verification_reads(
                &mut entries,
                &mut bytes,
                artifact
                    .old_identity()
                    .ok_or_else(|| RecoveryBlockedReason::InvalidJournal {
                        message: "existing artifact has no old identity".to_owned(),
                    })?,
                old_reads,
            )?;
        }
        add_verification_reads(&mut entries, &mut bytes, artifact.new_identity(), new_reads)?;
    }
    budget.check_entries(entries)?;
    budget.check_bytes(bytes)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn add_verification_reads(
    entries: &mut u64,
    bytes: &mut u64,
    identity: &FileIdentity,
    count: u64,
) -> Result<(), BudgetError> {
    *entries = entries
        .checked_add(count)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "recovery verification entries",
        })?;
    *bytes = bytes
        .checked_add(identity.length().checked_mul(count).ok_or(
            BudgetError::ArithmeticOverflow {
                resource: "recovery verification bytes",
            },
        )?)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "recovery verification bytes",
        })?;
    Ok(())
}

enum OwnedCorruptionRepair {
    RestoreExisting { ordinal: usize, displace_new: bool },
    RestoreAbsence { ordinal: usize },
}

fn plan_owned_corruption_repairs(
    execution: &RecoveryExecutionPlan,
    observations: &[ArtifactObservation],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<OwnedCorruptionRepair>, ObservationError> {
    if observations.len() != execution.artifacts.len() {
        return Err(RecoveryBlockedReason::InvalidJournal {
            message: "owned-corruption observations are incomplete".to_owned(),
        }
        .into());
    }
    let count = observations
        .iter()
        .filter(|artifact| artifact.has_repairable_owned_corruption())
        .count();
    let mut repairs = recovery_vec(count, "owned-corruption repair plan", budget)?;
    for (ordinal, (paths, observation)) in execution.artifacts.iter().zip(observations).enumerate()
    {
        if !observation.has_repairable_owned_corruption() {
            continue;
        }
        if observation.had_original {
            if paths.backup.is_none() || paths.old_identity.is_none() {
                return Err(RecoveryBlockedReason::InvalidJournal {
                    message: "owned-corruption repair has no captured old image".to_owned(),
                }
                .into());
            }
            repairs.push(OwnedCorruptionRepair::RestoreExisting {
                ordinal,
                displace_new: matches!(
                    observation.target,
                    EntryEvidence::New | EntryEvidence::CorruptNew
                ),
            });
        } else {
            repairs.push(OwnedCorruptionRepair::RestoreAbsence { ordinal });
        }
    }
    Ok(repairs)
}

fn recovery_join(
    root: &Path,
    relative: &JournalPath,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, ObservationError> {
    recovery_join_component(root, OsStr::new(relative.as_str()), resource, budget)
}

fn recovery_join_component(
    root: &Path,
    component: &OsStr,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, ObservationError> {
    let requested = root
        .as_os_str()
        .len()
        .checked_add(component.len())
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(
        u64::try_from(requested).map_err(|_| BudgetError::ArithmeticOverflow { resource })?,
    )?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(requested)
        .map_err(|error| RecoveryBlockedReason::Io {
            message: format!("failed to reserve {resource}: {error}"),
        })?;
    path.push(root);
    path.push(component);
    let actual =
        u64::try_from(path.capacity()).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(path)
}

fn execute_owned_corruption_repairs(
    journal: &Journal,
    execution: &RecoveryExecutionPlan,
    repairs: &[OwnedCorruptionRepair],
) -> Result<(), ExecutionError> {
    for repair in repairs {
        match repair {
            OwnedCorruptionRepair::RestoreExisting {
                ordinal,
                displace_new,
            } => {
                let paths = execution.artifacts.get(*ordinal).ok_or_else(|| {
                    ExecutionError::Blocked(invalid_event(
                        "owned-corruption repair ordinal is outside its execution plan",
                    ))
                })?;
                let backup = paths.backup.as_ref().ok_or_else(|| {
                    ExecutionError::Blocked(invalid_event(
                        "owned-corruption repair has no captured old path",
                    ))
                })?;
                let old_identity = paths.old_identity.as_ref().ok_or_else(|| {
                    ExecutionError::Blocked(invalid_event(
                        "owned-corruption repair has no captured old identity",
                    ))
                })?;
                if *displace_new {
                    capture_external_regular_in_journal_directory(
                        &paths.target,
                        journal.stage_directory(),
                        &paths.staging,
                        &paths.new_identity,
                        None,
                        &paths.target_parent_identity,
                    )?;
                }
                promote_journal_regular_to_external(
                    journal.backup_directory(),
                    backup,
                    &paths.target,
                    old_identity,
                    paths.old_digest,
                    &paths.target_parent_identity,
                )?;
            }
            OwnedCorruptionRepair::RestoreAbsence { ordinal } => {
                let paths = execution.artifacts.get(*ordinal).ok_or_else(|| {
                    ExecutionError::Blocked(invalid_event(
                        "owned-corruption repair ordinal is outside its execution plan",
                    ))
                })?;
                capture_external_regular_in_journal_directory(
                    &paths.target,
                    journal.stage_directory(),
                    &paths.staging,
                    &paths.new_identity,
                    None,
                    &paths.target_parent_identity,
                )?;
            }
        }
    }
    Ok(())
}

fn roll_forward(
    journal: &mut Journal,
    facts: &mut EventFacts,
    observations: &mut [ArtifactObservation],
    execution: &RecoveryExecutionPlan,
    event_plan: &mut JournalEventPlan,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExecutionError> {
    if facts.artifacts.len() != execution.artifacts.len()
        || observations.len() != execution.artifacts.len()
    {
        return Err(ExecutionError::Blocked(invalid_event(
            "recovery execution plan does not cover every artifact",
        )));
    }
    append_once(
        journal,
        &mut facts.staging_verified,
        JournalEventKey::StagingVerified,
        event_plan,
    )?;
    append_once(
        journal,
        &mut facts.journaled,
        JournalEventKey::Journaled,
        event_plan,
    )?;

    for index in 0..execution.artifacts.len() {
        forward_artifact(
            journal,
            facts,
            observations,
            execution,
            index,
            event_plan,
            budget,
        )?;
    }
    if observations.iter().any(|artifact| !artifact.is_published()) {
        return Err(ExecutionError::Blocked(
            RecoveryBlockedReason::UnexpectedEvidence {
                artifact: "publication-set".to_owned(),
            },
        ));
    }
    append_once(
        journal,
        &mut facts.published,
        JournalEventKey::Published,
        event_plan,
    )?;
    Ok(())
}

fn finish_published_events(
    journal: &mut Journal,
    facts: &mut EventFacts,
    event_plan: &mut JournalEventPlan,
) -> Result<(), ExecutionError> {
    append_once(
        journal,
        &mut facts.staging_verified,
        JournalEventKey::StagingVerified,
        event_plan,
    )?;
    append_once(
        journal,
        &mut facts.journaled,
        JournalEventKey::Journaled,
        event_plan,
    )?;
    for index in 0..journal.manifest().artifacts().len() {
        let artifact = &journal.manifest().artifacts()[index];
        if !facts.artifacts[index].promotion_intent {
            return Err(ExecutionError::Blocked(invalid_event(
                "promoted target has no durable intent",
            )));
        }
        if artifact.old_digest().is_some() && !facts.artifacts[index].backup_captured {
            return Err(ExecutionError::Blocked(invalid_event(
                "promoted replacement has no captured backup event",
            )));
        }
        append_artifact_once(
            journal,
            &mut facts.artifacts[index].promoted,
            u32::try_from(index).map_err(|_| {
                ExecutionError::Blocked(invalid_event("artifact ordinal overflowed"))
            })?,
            ArtifactEvent::Promoted,
            event_plan,
        )?;
    }
    append_once(
        journal,
        &mut facts.published,
        JournalEventKey::Published,
        event_plan,
    )?;
    Ok(())
}

fn forward_artifact(
    journal: &mut Journal,
    facts: &mut EventFacts,
    observations: &mut [ArtifactObservation],
    execution: &RecoveryExecutionPlan,
    index: usize,
    event_plan: &mut JournalEventPlan,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExecutionError> {
    let paths = execution.artifacts.get(index).ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "recovery execution ordinal is outside its execution plan",
        ))
    })?;
    let observation = *observations.get(index).ok_or_else(|| {
        ExecutionError::Blocked(invalid_event("recovery execution observation is missing"))
    })?;
    let facts = facts.artifacts.get_mut(index).ok_or_else(|| {
        ExecutionError::Blocked(invalid_event("recovery execution facts are missing"))
    })?;

    if let Some(old) = paths.old_digest {
        let old_identity = paths.old_identity.as_ref().ok_or_else(|| {
            ExecutionError::Blocked(invalid_event(
                "recovery existing artifact has no old identity",
            ))
        })?;
        let backup = paths.backup.as_ref().ok_or_else(|| {
            ExecutionError::Blocked(invalid_event(
                "recovery existing artifact has no backup path",
            ))
        })?;
        match (observation.target, observation.staging, observation.backup) {
            (EntryEvidence::Old, EntryEvidence::New, EntryEvidence::Missing) => {
                verify_digest_precharged(
                    &paths.target,
                    old,
                    old_identity,
                    &paths.target_parent_identity,
                )?;
                append_artifact_once(
                    journal,
                    &mut facts.backup_intent,
                    paths.ordinal,
                    ArtifactEvent::BackupIntent,
                    event_plan,
                )?;
                capture_external_regular_in_journal_directory(
                    &paths.target,
                    journal.backup_directory(),
                    backup,
                    old_identity,
                    Some(old),
                    &paths.target_parent_identity,
                )?;
                verify_journal_digest_precharged(
                    journal.backup_directory(),
                    backup,
                    old,
                    old_identity,
                )?;
                observations[index].target = EntryEvidence::Missing;
                observations[index].backup = EntryEvidence::Old;
                copy_security_metadata_between_journal_directories(
                    journal.backup_directory(),
                    backup,
                    journal.stage_directory(),
                    &paths.staging,
                    old_identity,
                    &paths.new_identity,
                    budget,
                )
                .map_err(map_security_metadata_execution_error)?;
                append_artifact_once(
                    journal,
                    &mut facts.backup_captured,
                    paths.ordinal,
                    ArtifactEvent::BackupCaptured,
                    event_plan,
                )?;
            }
            (EntryEvidence::Missing, EntryEvidence::New, EntryEvidence::Old) => {
                if !facts.backup_intent {
                    return Err(ExecutionError::Blocked(invalid_event(
                        "captured backup has no durable intent",
                    )));
                }
                verify_journal_digest_precharged(
                    journal.backup_directory(),
                    backup,
                    old,
                    old_identity,
                )?;
                copy_security_metadata_between_journal_directories(
                    journal.backup_directory(),
                    backup,
                    journal.stage_directory(),
                    &paths.staging,
                    old_identity,
                    &paths.new_identity,
                    budget,
                )
                .map_err(map_security_metadata_execution_error)?;
                append_artifact_once(
                    journal,
                    &mut facts.backup_captured,
                    paths.ordinal,
                    ArtifactEvent::BackupCaptured,
                    event_plan,
                )?;
            }
            (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::Old) => {
                if !facts.promotion_intent {
                    return Err(ExecutionError::Blocked(invalid_event(
                        "promoted target has no durable intent",
                    )));
                }
                verify_digest_precharged(
                    &paths.target,
                    paths.new_digest,
                    &paths.new_identity,
                    &paths.target_parent_identity,
                )?;
                append_artifact_once(
                    journal,
                    &mut facts.promoted,
                    paths.ordinal,
                    ArtifactEvent::Promoted,
                    event_plan,
                )?;
                return Ok(());
            }
            _ => return Err(unexpected_execution_artifact(paths.ordinal)),
        }
    } else {
        match (observation.target, observation.staging, observation.backup) {
            (EntryEvidence::Missing, EntryEvidence::New, EntryEvidence::Missing) => {}
            (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::Missing) => {
                if !facts.promotion_intent {
                    return Err(ExecutionError::Blocked(invalid_event(
                        "promoted target has no durable intent",
                    )));
                }
                verify_digest_precharged(
                    &paths.target,
                    paths.new_digest,
                    &paths.new_identity,
                    &paths.target_parent_identity,
                )?;
                append_artifact_once(
                    journal,
                    &mut facts.promoted,
                    paths.ordinal,
                    ArtifactEvent::Promoted,
                    event_plan,
                )?;
                return Ok(());
            }
            _ => return Err(unexpected_execution_artifact(paths.ordinal)),
        }
    }

    // Revalidate from a fresh no-follow handle immediately before persisting promotion intent.
    // Existing corruption is rejected before the target is renamed.
    verify_journal_digest_precharged(
        journal.stage_directory(),
        &paths.staging,
        paths.new_digest,
        &paths.new_identity,
    )?;
    append_artifact_once(
        journal,
        &mut facts.promotion_intent,
        paths.ordinal,
        ArtifactEvent::PromotionIntent,
        event_plan,
    )?;
    promote_journal_regular_to_external(
        journal.stage_directory(),
        &paths.staging,
        &paths.target,
        &paths.new_identity,
        Some(paths.new_digest),
        &paths.target_parent_identity,
    )?;
    verify_digest_precharged(
        &paths.target,
        paths.new_digest,
        &paths.new_identity,
        &paths.target_parent_identity,
    )?;
    observations[index].target = EntryEvidence::New;
    observations[index].staging = EntryEvidence::Missing;
    append_artifact_once(
        journal,
        &mut facts.promoted,
        paths.ordinal,
        ArtifactEvent::Promoted,
        event_plan,
    )?;
    Ok(())
}

fn capture_recovery_target_into_stage(
    journal: &Journal,
    paths: &RecoveryArtifactExecution,
    expected_digest: Option<DigestV1>,
) -> Result<(), ExecutionError> {
    capture_external_regular_in_journal_directory(
        &paths.target,
        journal.stage_directory(),
        &paths.staging,
        &paths.new_identity,
        expected_digest,
        &paths.target_parent_identity,
    )?;
    Ok(())
}

fn restore_recovery_backup_to_target(
    journal: &Journal,
    paths: &RecoveryArtifactExecution,
    backup: &Path,
    expected_identity: &FileIdentity,
    expected_digest: Option<DigestV1>,
) -> Result<(), ExecutionError> {
    promote_journal_regular_to_external(
        journal.backup_directory(),
        backup,
        &paths.target,
        expected_identity,
        expected_digest,
        &paths.target_parent_identity,
    )?;
    Ok(())
}

fn roll_back(
    journal: &Journal,
    observations: &mut [ArtifactObservation],
    execution: &RecoveryExecutionPlan,
) -> Result<(), ExecutionError> {
    if observations.len() != execution.artifacts.len() {
        return Err(ExecutionError::Blocked(invalid_event(
            "rollback execution plan does not cover every artifact",
        )));
    }
    for index in (0..execution.artifacts.len()).rev() {
        let paths = &execution.artifacts[index];
        let observation = observations[index];
        if let Some(old) = paths.old_digest {
            let backup = paths.backup.as_ref().ok_or_else(|| {
                ExecutionError::Blocked(invalid_event(
                    "rollback existing artifact has no backup path",
                ))
            })?;
            let old_identity = paths.old_identity.as_ref().ok_or_else(|| {
                ExecutionError::Blocked(invalid_event(
                    "rollback existing artifact has no old identity",
                ))
            })?;
            match (observation.target, observation.staging, observation.backup) {
                (EntryEvidence::Old, _, EntryEvidence::Missing) => {}
                (EntryEvidence::Missing, _, EntryEvidence::Old) => {
                    restore_recovery_backup_to_target(
                        journal,
                        paths,
                        backup,
                        old_identity,
                        Some(old),
                    )?;
                    verify_digest_precharged(
                        &paths.target,
                        old,
                        old_identity,
                        &paths.target_parent_identity,
                    )?;
                    observations[index].target = EntryEvidence::Old;
                    observations[index].backup = EntryEvidence::Missing;
                }
                (
                    EntryEvidence::New | EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::Old,
                ) => {
                    capture_recovery_target_into_stage(
                        journal,
                        paths,
                        (observation.target == EntryEvidence::New).then_some(paths.new_digest),
                    )?;
                    restore_recovery_backup_to_target(
                        journal,
                        paths,
                        backup,
                        old_identity,
                        Some(old),
                    )?;
                    verify_digest_precharged(
                        &paths.target,
                        old,
                        old_identity,
                        &paths.target_parent_identity,
                    )?;
                    observations[index].target = EntryEvidence::Old;
                    observations[index].staging = observation.target;
                    observations[index].backup = EntryEvidence::Missing;
                }
                (EntryEvidence::Missing, _, EntryEvidence::CorruptOld) => {
                    restore_recovery_backup_to_target(journal, paths, backup, old_identity, None)?;
                    observations[index].target = EntryEvidence::CorruptOld;
                    observations[index].backup = EntryEvidence::Missing;
                    return Err(unexpected_execution_artifact(paths.ordinal));
                }
                (
                    EntryEvidence::New | EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::CorruptOld,
                ) => {
                    capture_recovery_target_into_stage(
                        journal,
                        paths,
                        (observation.target == EntryEvidence::New).then_some(paths.new_digest),
                    )?;
                    restore_recovery_backup_to_target(journal, paths, backup, old_identity, None)?;
                    observations[index].target = EntryEvidence::CorruptOld;
                    observations[index].staging = observation.target;
                    observations[index].backup = EntryEvidence::Missing;
                    return Err(unexpected_execution_artifact(paths.ordinal));
                }
                _ => return Err(unexpected_execution_artifact(paths.ordinal)),
            }
        } else {
            match (observation.target, observation.staging, observation.backup) {
                (EntryEvidence::Missing, _, EntryEvidence::Missing) => {}
                (
                    EntryEvidence::New | EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::Missing,
                ) => {
                    capture_recovery_target_into_stage(
                        journal,
                        paths,
                        (observation.target == EntryEvidence::New).then_some(paths.new_digest),
                    )?;
                    observations[index].target = EntryEvidence::Missing;
                    observations[index].staging = observation.target;
                }
                _ => return Err(unexpected_execution_artifact(paths.ordinal)),
            }
        }
    }
    if observations
        .iter()
        .any(|artifact| !artifact.is_rolled_back())
    {
        return Err(ExecutionError::Blocked(
            RecoveryBlockedReason::UnexpectedEvidence {
                artifact: "rollback-set".to_owned(),
            },
        ));
    }
    Ok(())
}

fn unexpected_execution_artifact(ordinal: u32) -> ExecutionError {
    ExecutionError::Blocked(RecoveryBlockedReason::UnexpectedEvidence {
        artifact: format!("artifact-{ordinal:08}"),
    })
}

#[derive(Debug, Clone, Copy)]
enum ArtifactEvent {
    BackupIntent,
    BackupCaptured,
    PromotionIntent,
    Promoted,
}

fn append_artifact_once(
    journal: &mut Journal,
    value: &mut bool,
    ordinal: u32,
    event: ArtifactEvent,
    event_plan: &mut JournalEventPlan,
) -> Result<(), super::journal::JournalError> {
    let key = match event {
        ArtifactEvent::BackupIntent => JournalEventKey::BackupIntent(ordinal),
        ArtifactEvent::BackupCaptured => JournalEventKey::BackupCaptured(ordinal),
        ArtifactEvent::PromotionIntent => JournalEventKey::PromotionIntent(ordinal),
        ArtifactEvent::Promoted => JournalEventKey::Promoted(ordinal),
    };
    append_once(journal, value, key, event_plan)
}

fn append_once(
    journal: &mut Journal,
    value: &mut bool,
    event: JournalEventKey,
    event_plan: &mut JournalEventPlan,
) -> Result<(), super::journal::JournalError> {
    if !*value {
        journal.append_planned(event_plan, event)?;
        *value = true;
    }
    Ok(())
}

fn verify_digest_precharged(
    path: &Path,
    expected: DigestV1,
    expected_identity: &FileIdentity,
    expected_parent: &DirectoryIdentity,
) -> Result<(), ExecutionError> {
    let mut file = match open_readonly_regular_in_parent(path, expected_parent) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) =>
        {
            return Err(unexpected_verification());
        }
        Err(error) => return Err(ExecutionError::Io(error)),
    };
    let identity = opened_file_identity(&file)?;
    if &identity != expected_identity {
        return Err(unexpected_verification());
    }
    let actual = DigestV1::hash_reader(&mut file, expected_identity.length())?;
    if actual == expected {
        Ok(())
    } else {
        Err(unexpected_verification())
    }
}

fn verify_journal_digest_precharged(
    directory: &JournalDirectory,
    path: &Path,
    expected: DigestV1,
    expected_identity: &FileIdentity,
) -> Result<(), ExecutionError> {
    let mut file = match open_journal_regular_in_directory(directory, path) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) =>
        {
            return Err(unexpected_verification());
        }
        Err(error) => return Err(ExecutionError::Io(error)),
    };
    let identity = opened_file_identity(&file)?;
    if &identity != expected_identity {
        return Err(unexpected_verification());
    }
    let actual = DigestV1::hash_reader(&mut file, expected_identity.length())?;
    if actual == expected {
        Ok(())
    } else {
        Err(unexpected_verification())
    }
}

fn unexpected_verification() -> ExecutionError {
    ExecutionError::Blocked(RecoveryBlockedReason::UnexpectedEvidence {
        artifact: "post-move-verification".to_owned(),
    })
}

#[derive(Debug, Error)]
enum ExecutionError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Journal(#[from] super::journal::JournalError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Blocked(RecoveryBlockedReason),
}

fn block_and_record<T>(
    journal: &mut Journal,
    locator: &RecoveryLocator,
    reason: RecoveryBlockedReason,
    budget: &mut AssetLoadBudget,
) -> Result<T, RecoveryError> {
    let terminal = journal
        .events()
        .iter()
        .any(|event| matches!(event.kind(), JournalEventKind::Finalized));
    if !terminal
        && !journal
            .events()
            .iter()
            .any(|event| matches!(event.kind(), JournalEventKind::RecoveryBlocked { .. }))
    {
        let record = reason.to_string();
        journal
            .append(JournalEventKind::RecoveryBlocked { reason: record }, budget)
            .map_err(|error| map_journal_mutation_error(locator, error))?;
    }
    Err(blocked(locator, reason))
}

fn blocked(locator: &RecoveryLocator, reason: RecoveryBlockedReason) -> RecoveryError {
    RecoveryError::Blocked {
        locator: Box::new(locator.clone()),
        reason: Box::new(reason),
    }
}

fn map_commit_guard_error(locator: &RecoveryLocator, error: io::Error) -> RecoveryError {
    if error.kind() == io::ErrorKind::WouldBlock {
        RecoveryError::Busy {
            locator: Box::new(locator.clone()),
            message: error.to_string(),
        }
    } else {
        blocked(locator, io_reason(error))
    }
}

fn invalid_journal(message: String) -> RecoveryBlockedReason {
    RecoveryBlockedReason::InvalidJournal { message }
}

fn map_journal_open_error(locator: &RecoveryLocator, error: JournalError) -> RecoveryError {
    match error {
        JournalError::Budget(source) => RecoveryError::Budget {
            locator: Box::new(locator.clone()),
            source,
        },
        JournalError::Io(error) => blocked(locator, io_reason(error)),
        error => blocked(locator, invalid_journal(error.to_string())),
    }
}

fn map_journal_mutation_error(locator: &RecoveryLocator, error: JournalError) -> RecoveryError {
    match error {
        JournalError::Budget(source) => RecoveryError::Budget {
            locator: Box::new(locator.clone()),
            source,
        },
        JournalError::Io(error) => blocked(locator, io_reason(error)),
        error => blocked(locator, invalid_journal(error.to_string())),
    }
}

fn map_baseline_error(
    locator: &RecoveryLocator,
    error: super::baseline::BaselineBuildError,
) -> RecoveryError {
    match error.into_budget() {
        Ok(source) => RecoveryError::Budget {
            locator: Box::new(locator.clone()),
            source,
        },
        Err(super::baseline::BaselineBuildError::Revision { expected, actual }) => blocked(
            locator,
            RecoveryBlockedReason::BaselineUnavailable { expected, actual },
        ),
        Err(error) => blocked(
            locator,
            RecoveryBlockedReason::BaselineRebuild {
                message: error.to_string(),
            },
        ),
    }
}

fn map_observation_error(locator: &RecoveryLocator, error: ObservationError) -> RecoveryError {
    match error {
        ObservationError::Budget(source) => RecoveryError::Budget {
            locator: Box::new(locator.clone()),
            source,
        },
        ObservationError::Blocked(reason) => blocked(locator, reason),
    }
}

fn map_execution_error(locator: &RecoveryLocator, error: ExecutionError) -> RecoveryError {
    match error {
        ExecutionError::Budget(source) => RecoveryError::Budget {
            locator: Box::new(locator.clone()),
            source,
        },
        ExecutionError::Blocked(reason) => blocked(locator, reason),
        ExecutionError::Journal(error) => map_journal_mutation_error(locator, error),
        ExecutionError::Io(error) => blocked(
            locator,
            RecoveryBlockedReason::Io {
                message: error.to_string(),
            },
        ),
    }
}

fn map_security_metadata_execution_error(error: SecurityMetadataError) -> ExecutionError {
    match error {
        SecurityMetadataError::Budget(source) => ExecutionError::Budget(source),
        SecurityMetadataError::Io(error) => ExecutionError::Io(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tempfile::TempDir;
    use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};

    use crate::workspace::{
        AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationValue, PlanPayload,
        PrepareOptions, PublicationTarget, SourceExpectation, SourceOpenRequest, WorkspaceLookup,
        WorkspaceOptions, WorkspaceView,
    };
    use crate::{
        AssetLoadBudget, FieldPath, ObjectAddress, SourceAlias, SourceFingerprint, SourceKind,
        SourceLocator, UnityClass, UnityValue,
    };

    use super::*;

    const SOURCE_ALIAS: &str = "recovery.prefab";
    const RESOURCE_ALIAS: &str = "recovery-audio.asset";
    const RESOURCE_PAYLOAD: &[u8] = b"recoverable streamed payload";
    const CRASH_ROOT_ENV: &str = "UNITY_ASSET_TEST_CRASH_ROOT";
    const CRASH_SCENARIO_ENV: &str = "UNITY_ASSET_TEST_CRASH_SCENARIO";
    const RESOURCE_CRASH_SCENARIO: &str = "resource";
    const RECOVERY_CRASH_SCENARIO: &str = "recovery";
    const CRASH_CHILD_TEST: &str = "workspace::commit::recovery::tests::publication_crash_child";
    const CRASH_WORKSPACE_ID: u128 = 0x7a11_5afe_c0de_0001;
    const YAML: &[u8] =
        b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";
    const RESOURCE_YAML: &[u8] = b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!83 &1\nAudioClip:\n  m_StreamData: {path: old.resS, offset: 7, size: 4}\n";

    fn test_layout_from_locator(locator: &RecoveryLocator) -> Result<JournalLayout, RecoveryError> {
        layout_from_locator(locator, &mut AssetLoadBudget::default())
    }

    fn name_path() -> FieldPath {
        FieldPath::root().push_field("m_Name").expect("field path")
    }

    fn address() -> ObjectAddress {
        ObjectAddress::yaml(
            SourceLocator::path(SOURCE_ALIAS).expect("source locator"),
            "1",
        )
        .expect("object address")
    }

    fn guard(value: &str) -> FieldGuard {
        let class = UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
        let path = name_path();
        let value = UnityValue::String(value.to_owned());
        let mut budget = AssetLoadBudget::default();
        FieldGuard::new(
            yaml_field_schema_digest(&class, &path, &value, &mut budget).expect("schema digest"),
            semantic_value_digest(&value, &mut budget).expect("value digest"),
        )
    }

    fn mutation_plan(workspace: &AssetWorkspace, before: &str, after: &str) -> MutationPlan {
        MutationPlan::new(
            workspace.revision(),
            vec![SourceExpectation::new(
                SourceLocator::path(SOURCE_ALIAS).expect("source locator"),
                SourceFingerprint::from_bytes(SourceKind::Yaml, YAML),
            )],
            Vec::new(),
            vec![GenericMutation::FieldReplace {
                target: address(),
                path: name_path(),
                guard: guard(before),
                replacement: MutationValue::string(after).expect("mutation value"),
            }],
        )
        .expect("mutation plan")
    }

    fn resource_address() -> ObjectAddress {
        ObjectAddress::yaml(
            SourceLocator::path(RESOURCE_ALIAS).expect("resource locator"),
            "1",
        )
        .expect("resource address")
    }

    fn resource_path() -> FieldPath {
        FieldPath::root()
            .push_field("m_StreamData")
            .expect("resource field path")
    }

    fn resource_plan(workspace: &AssetWorkspace) -> MutationPlan {
        let snapshot = workspace.snapshot();
        let path = resource_path();
        let mut budget = AssetLoadBudget::default();
        let WorkspaceLookup::Resolved(handle) = snapshot
            .resolve_object(&resource_address(), &mut budget)
            .expect("resolve AudioClip")
        else {
            panic!("AudioClip must resolve");
        };
        let object = snapshot
            .read_object(&handle, &mut budget)
            .expect("read AudioClip");
        let current = object.class().value_at_path(&path).expect("resource field");
        let guard = FieldGuard::new(
            yaml_field_schema_digest(object.class(), &path, current, &mut budget)
                .expect("resource schema digest"),
            semantic_value_digest(current, &mut budget).expect("resource value digest"),
        );
        let payload = PlanPayload::new(RESOURCE_PAYLOAD.to_vec());
        MutationPlan::new(
            workspace.revision(),
            vec![SourceExpectation::new(
                SourceLocator::path(RESOURCE_ALIAS).expect("resource locator"),
                SourceFingerprint::from_bytes(SourceKind::Yaml, RESOURCE_YAML),
            )],
            vec![payload.clone()],
            vec![GenericMutation::ResourceReplace {
                target: resource_address(),
                path,
                guard,
                payload: payload.digest(),
            }],
        )
        .expect("resource mutation plan")
    }

    fn committed_fixture() -> (TempDir, std::path::PathBuf, AssetWorkspace, CommitReport) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(SOURCE_ALIAS);
        fs::write(&path, YAML).expect("fixture bytes");
        let mut workspace = AssetWorkspace::new().expect("workspace");
        workspace
            .load_source(
                SourceOpenRequest::new(&path, SourceAlias::new(SOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load fixture");
        let prepared = workspace
            .prepare(
                mutation_plan(&workspace, "Before", "After"),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare mutation");
        let report = workspace
            .commit(
                prepared,
                PublicationTarget::in_place(directory.path()).expect("publication target"),
                &mut AssetLoadBudget::default(),
            )
            .expect("commit fixture");
        (directory, path, workspace, report)
    }

    fn crash_workspace_id() -> WorkspaceId {
        WorkspaceId::from_u128(CRASH_WORKSPACE_ID).expect("workspace id")
    }

    fn open_crash_workspace(path: &Path, workspace_id: WorkspaceId) -> AssetWorkspace {
        let mut workspace =
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default())
                .expect("crash workspace");
        workspace
            .load_source(
                SourceOpenRequest::new(path, SourceAlias::new(SOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load crash fixture");
        workspace
    }

    fn open_crash_resource_workspace(path: &Path, workspace_id: WorkspaceId) -> AssetWorkspace {
        let mut workspace =
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default())
                .expect("crash resource workspace");
        workspace
            .load_source(
                SourceOpenRequest::new(
                    path,
                    SourceAlias::new(RESOURCE_ALIAS).expect("resource alias"),
                )
                .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load crash resource fixture");
        workspace
    }

    fn crash_locator(directory: &Path, path: &Path) -> RecoveryLocator {
        let workspace = open_crash_workspace(path, crash_workspace_id());
        let prepared = workspace
            .prepare(
                mutation_plan(&workspace, "Before", "After"),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare crash transaction");
        let target = PublicationTarget::in_place(directory).expect("publication target");
        let preflight =
            super::super::preflight_commit(&prepared, &target, &mut AssetLoadBudget::default())
                .expect("preflight crash transaction");
        target.recovery_locator(preflight.transaction)
    }

    fn crash_resource_locator(directory: &Path, path: &Path) -> RecoveryLocator {
        let workspace = open_crash_resource_workspace(path, crash_workspace_id());
        let prepared = workspace
            .prepare(
                resource_plan(&workspace),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare crash resource transaction");
        let target = PublicationTarget::in_place(directory).expect("publication target");
        let preflight =
            super::super::preflight_commit(&prepared, &target, &mut AssetLoadBudget::default())
                .expect("preflight crash resource transaction");
        target.recovery_locator(preflight.transaction)
    }

    fn run_crash_child(directory: &Path, point: &str, scenario: Option<&str>) {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg(CRASH_CHILD_TEST)
            .arg("--nocapture")
            .env(CRASH_ROOT_ENV, directory)
            .env(super::super::TEST_CRASH_POINT_ENV, point);
        if let Some(scenario) = scenario {
            command.env(CRASH_SCENARIO_ENV, scenario);
        }
        let output = command.output().expect("spawn crashing commit");
        assert_eq!(
            output.status.code(),
            Some(86),
            "failpoint {point} did not terminate at the requested barrier\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn run_crashing_commit(point: &str) -> (TempDir, PathBuf, RecoveryLocator) {
        let directory = tempfile::tempdir().expect("crash directory");
        let path = directory.path().join(SOURCE_ALIAS);
        fs::write(&path, YAML).expect("crash fixture bytes");
        let locator = crash_locator(directory.path(), &path);
        run_crash_child(directory.path(), point, None);
        (directory, path, locator)
    }

    fn run_crashing_resource_commit(point: &str) -> (TempDir, PathBuf, RecoveryLocator) {
        let directory = tempfile::tempdir().expect("resource crash directory");
        let path = directory.path().join(RESOURCE_ALIAS);
        fs::write(&path, RESOURCE_YAML).expect("resource crash fixture bytes");
        let locator = crash_resource_locator(directory.path(), &path);
        run_crash_child(directory.path(), point, Some(RESOURCE_CRASH_SCENARIO));
        (directory, path, locator)
    }

    #[test]
    fn publication_crash_child() {
        let Some(root) = std::env::var_os(CRASH_ROOT_ENV) else {
            return;
        };
        let root = PathBuf::from(root);
        if std::env::var(CRASH_SCENARIO_ENV)
            .is_ok_and(|scenario| scenario == RECOVERY_CRASH_SCENARIO)
        {
            let path = root.join(SOURCE_ALIAS);
            let locator = crash_locator(&root, &path);
            let result = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default());
            panic!("configured recovery crash point was not reached: {result:?}");
        }
        let (mut workspace, prepared) = if std::env::var(CRASH_SCENARIO_ENV)
            .is_ok_and(|scenario| scenario == RESOURCE_CRASH_SCENARIO)
        {
            let path = root.join(RESOURCE_ALIAS);
            let workspace = open_crash_resource_workspace(&path, crash_workspace_id());
            let prepared = workspace
                .prepare(
                    resource_plan(&workspace),
                    PrepareOptions::default(),
                    &mut AssetLoadBudget::default(),
                )
                .expect("prepare child resource transaction");
            (workspace, prepared)
        } else {
            let path = root.join(SOURCE_ALIAS);
            let workspace = open_crash_workspace(&path, crash_workspace_id());
            let prepared = workspace
                .prepare(
                    mutation_plan(&workspace, "Before", "After"),
                    PrepareOptions::default(),
                    &mut AssetLoadBudget::default(),
                )
                .expect("prepare child transaction");
            (workspace, prepared)
        };
        let result = workspace.commit(
            prepared,
            PublicationTarget::in_place(&root).expect("child publication target"),
            &mut AssetLoadBudget::default(),
        );
        panic!("configured crash point was not reached: {result:?}");
    }

    #[test]
    fn crash_before_preparation_installation_leaves_no_recovery_blocker() {
        let (directory, path, locator) = run_crashing_commit("preparation_temporary_synced");
        let layout = test_layout_from_locator(&locator).expect("preparation-attempt layout");
        assert_target_unchanged(&path, YAML);
        assert!(!layout.preparation_path().exists());
        assert!(!layout.directory().exists());
        assert!(layout.preparation_temporary_path().is_file());
        let mut workspace = open_crash_workspace(&path, crash_workspace_id());
        let prepared = workspace
            .prepare(
                mutation_plan(&workspace, "Before", "After"),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare after orphaned attempt");

        workspace
            .commit(
                prepared,
                PublicationTarget::in_place(directory.path()).expect("retry target"),
                &mut AssetLoadBudget::default(),
            )
            .expect("orphaned preparation attempt must not block a new canonical attempt");

        assert!(!layout.preparation_temporary_path().exists());
        assert_eq!(read_name(&workspace), "After");
    }

    #[test]
    fn preopen_recovery_removes_an_orphaned_preparation_attempt() {
        let (_directory, path, locator) = run_crashing_commit("preparation_temporary_synced");
        let layout = test_layout_from_locator(&locator).expect("preparation-attempt layout");
        assert!(layout.preparation_temporary_path().is_file());

        let outcome = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("clean orphaned preparation attempt");

        assert_eq!(outcome, RecoveryOutcome::NoTransaction(locator));
        assert!(!layout.preparation_temporary_path().exists());
        assert_target_unchanged(&path, YAML);
    }

    #[test]
    fn replaced_orphaned_preparation_attempt_is_preserved() {
        let (_directory, path, locator) = run_crashing_commit("preparation_temporary_synced");
        let layout = test_layout_from_locator(&locator).expect("preparation-attempt layout");
        let attempt = layout.preparation_temporary_path();
        fs::remove_file(attempt).expect("replace owned preparation attempt");
        fs::create_dir(attempt).expect("external replacement directory");

        let error = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect_err("external replacement must block cleanup");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { artifact })
                if artifact == "preparation-attempt-path"
        ));
        assert!(attempt.is_dir());
        assert_target_unchanged(&path, YAML);
    }

    #[test]
    fn clean_publication_root_reports_no_transaction() {
        let directory = tempfile::tempdir().expect("clean publication root");
        let transaction = unity_asset_core::TransactionId::new(DigestV1::hash_bytes(b"absent"));
        let locator = PublicationTarget::in_place(directory.path())
            .expect("publication target")
            .recovery_locator(transaction);

        let recovered = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("clean recovery probe");
        let abandoned = AssetWorkspace::abandon_at(&locator, &mut AssetLoadBudget::default())
            .expect("clean abandon probe");

        assert_eq!(recovered, RecoveryOutcome::NoTransaction(locator.clone()));
        assert_eq!(abandoned, RecoveryOutcome::NoTransaction(locator));
    }

    #[test]
    fn lost_premanifest_rollback_response_redelivers_the_same_receipt() {
        let (directory, path, locator) = run_crashing_commit("recovery_baseline_synced");
        run_crash_child(
            directory.path(),
            "premanifest_rollback_recorded",
            Some(RECOVERY_CRASH_SCENARIO),
        );
        let layout = test_layout_from_locator(&locator).expect("recovery layout");
        assert!(!layout.preparation_path().exists());
        assert!(!layout.directory().exists());
        assert!(layout.rollback_path().is_file());
        assert_target_unchanged(&path, YAML);

        let first = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("redeliver lost rollback response");
        let second = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("redeliver rollback receipt again");

        assert_eq!(first, second);
        assert_eq!(first.workspace_id(), Some(crash_workspace_id()));
        assert!(first.rolled_back().is_some());
    }

    #[test]
    fn interrupted_premanifest_rollback_capture_reconciles_duplicate_preparation() {
        let (directory, path, locator) = run_crashing_commit("recovery_baseline_synced");
        run_crash_child(
            directory.path(),
            "premanifest_rollback_captured",
            Some(RECOVERY_CRASH_SCENARIO),
        );
        let layout = test_layout_from_locator(&locator).expect("recovery layout");
        assert!(layout.preparation_path().is_file());
        assert!(layout.rollback_path().is_file());
        assert!(!layout.directory().exists());

        let first = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("reconcile duplicate preparation record");
        let second = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("redeliver reconciled rollback receipt");

        assert_eq!(first, second);
        assert!(first.rolled_back().is_some());
        assert!(!layout.preparation_path().exists());
        assert!(layout.rollback_path().is_file());
        assert_target_unchanged(&path, YAML);
    }

    #[test]
    fn duplicate_premanifest_record_with_transaction_preserves_all_evidence() {
        let (_directory, path, locator) = run_crashing_commit("recovery_baseline_synced");
        let layout = test_layout_from_locator(&locator).expect("recovery layout");
        fs::copy(layout.preparation_path(), layout.rollback_path())
            .expect("copy preparation into rollback receipt");

        let error = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect_err("transaction evidence must block duplicate-record cleanup");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { artifact })
                if artifact == "transaction-directory"
        ));
        assert!(layout.preparation_path().is_file());
        assert!(layout.rollback_path().is_file());
        assert!(layout.directory().is_dir());
        assert_target_unchanged(&path, YAML);
    }

    #[test]
    fn child_crashes_before_final_manifest_roll_back_without_target_writes() {
        for point in [
            "preparation_installed",
            "transaction_directory_installed",
            "private_directories_synced",
            "staged_artifact_synced:0",
            "recovery_baseline_synced",
            "manifest_temporary_synced",
        ] {
            let (directory, path, locator) = run_crashing_commit(point);
            assert_target_unchanged(&path, YAML);
            if point == "preparation_installed" {
                assert!(!locator.root().exists());
                assert!(
                    test_layout_from_locator(&locator)
                        .expect("preparation-only layout")
                        .preparation_path()
                        .is_file()
                );
            }
            let outcome = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
                .unwrap_or_else(|error| panic!("recover {point}: {error}"));

            let receipt = outcome
                .rolled_back()
                .expect("premanifest recovery must return a rollback receipt")
                .clone();
            assert_eq!(receipt.workspace_id(), crash_workspace_id());
            assert_eq!(receipt.recovery(), &locator);
            assert!(!locator.root().exists(), "{point} transaction directory");
            assert!(
                !JournalLayout::new(
                    directory.path(),
                    locator.transaction(),
                    locator.root_identity().clone(),
                )
                .preparation_path()
                .exists(),
                "{point} preparation record"
            );
            assert!(
                JournalLayout::new(
                    directory.path(),
                    locator.transaction(),
                    locator.root_identity().clone(),
                )
                .rollback_path()
                .is_file(),
                "{point} rollback receipt"
            );
            assert_eq!(
                AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default(),)
                    .unwrap_or_else(|error| panic!("repeat recover {point}: {error}")),
                outcome,
                "{point} rollback receipt must redeliver idempotently"
            );
            let mut workspace = open_crash_workspace(&path, receipt.workspace_id());
            assert_eq!(workspace.revision(), receipt.base_revision());
            let prepared = workspace
                .prepare(
                    mutation_plan(&workspace, "Before", "After"),
                    PrepareOptions::default(),
                    &mut AssetLoadBudget::default(),
                )
                .expect("prepare after premanifest recovery");
            workspace
                .commit(
                    prepared,
                    PublicationTarget::in_place(directory.path()).expect("retry target"),
                    &mut AssetLoadBudget::default(),
                )
                .expect("commit after premanifest recovery");
            assert_eq!(read_name(&workspace), "After");
        }
    }

    #[test]
    fn premanifest_recovery_preserves_every_entry_when_unknown_evidence_exists() {
        let (_directory, path, locator) = run_crashing_commit("recovery_baseline_synced");
        let staged = locator.root().join(STAGE_DIRECTORY).join("00000000.stage");
        let staged_before = fs::read(&staged).expect("valid staged image");
        let baseline_directory = locator.root().join(BASELINE_DIRECTORY);
        let baseline_before: Vec<_> = fs::read_dir(&baseline_directory)
            .expect("baseline directory")
            .map(|entry| {
                let path = entry.expect("baseline entry").path();
                let bytes = fs::read(&path).expect("baseline image");
                (path, bytes)
            })
            .collect();
        let unknown = locator.root().join(STAGE_DIRECTORY).join("unknown.bin");
        fs::write(&unknown, b"external evidence").expect("unknown evidence");
        let preparation_layout = test_layout_from_locator(&locator).expect("premanifest layout");
        let preparation = preparation_layout.preparation_path();
        let error = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect_err("unknown premanifest evidence must block recovery");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_target_unchanged(&path, YAML);
        assert_eq!(
            fs::read(&unknown).expect("retained evidence"),
            b"external evidence"
        );
        assert_eq!(
            fs::read(&staged).expect("retained staged image"),
            staged_before
        );
        for (path, bytes) in baseline_before {
            assert_eq!(fs::read(path).expect("retained baseline image"), bytes);
        }
        assert!(locator.root().exists());
        assert!(preparation.exists());
        for child in [
            EVENTS_DIRECTORY,
            STAGE_DIRECTORY,
            BACKUP_DIRECTORY,
            BASELINE_DIRECTORY,
        ] {
            assert!(locator.root().join(child).exists(), "retained {child}");
        }
    }

    #[test]
    fn premanifest_recovery_rejects_a_declared_hard_link_without_deleting_it() {
        let (directory, path, locator) = run_crashing_commit("private_directories_synced");
        let external = directory.path().join("external-evidence.bin");
        fs::write(&external, b"shared evidence").expect("external evidence");
        let staged = locator.root().join(STAGE_DIRECTORY).join("00000000.stage");
        fs::hard_link(&external, &staged).expect("hard-linked staged evidence");
        let error = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect_err("hard-linked premanifest evidence must block recovery");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::Io { .. })
        ));
        assert_target_unchanged(&path, YAML);
        assert_eq!(
            fs::read(&external).expect("external hard link"),
            b"shared evidence"
        );
        assert_eq!(
            fs::read(&staged).expect("staged hard link"),
            b"shared evidence"
        );
    }

    #[test]
    fn child_crashes_after_final_manifest_redeliver_historical_commit_receipts() {
        for point in [
            "manifest_installed",
            "backup_intent:0",
            "backup_renamed:0",
            "promotion_intent:0",
            "promotion_renamed:0",
            "published",
            "baseline_cas_before_event",
            "finalized_before_response",
        ] {
            let (_directory, path, locator) = run_crashing_commit(point);
            let first = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
                .unwrap_or_else(|error| panic!("recover {point}: {error}"));
            let expected = first
                .committed()
                .expect("postmanifest recovery must return the canonical commit report")
                .clone();
            assert_eq!(expected.recovery(), &locator);
            let mut workspace = open_crash_workspace(&path, expected.workspace_id());
            assert_eq!(workspace.revision(), expected.committed_revision());
            assert_eq!(read_name(&workspace), "After");
            let finalized = workspace
                .finalize_recovery_at(&locator, &mut AssetLoadBudget::default())
                .unwrap_or_else(|error| panic!("finalize {point}: {error}"));
            assert_eq!(
                finalized,
                RecoveryOutcome::Finalized(Box::new(expected.clone()))
            );
            let redelivered = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
                .unwrap_or_else(|error| panic!("redeliver {point}: {error}"));
            assert_eq!(
                redelivered,
                RecoveryOutcome::HistoricalCommitReceipt(Box::new(expected))
            );
            assert!(redelivered.filesystem_recovered().is_none());
            assert!(redelivered.historical_commit_receipt().is_some());
        }
    }

    #[test]
    fn child_crashes_redeliver_historical_two_artifact_commit_receipts() {
        for point in [
            "backup_intent:1",
            "backup_renamed:1",
            "promotion_intent:0",
            "promotion_renamed:0",
            "promotion_intent:1",
            "promotion_renamed:1",
        ] {
            let (_directory, path, locator) = run_crashing_resource_commit(point);
            if point == "backup_renamed:1" {
                assert!(
                    matches!(
                        fs::symlink_metadata(&path),
                        Err(error) if error.kind() == io::ErrorKind::NotFound
                    ),
                    "the trusted root must genuinely be missing before recovery"
                );
            }

            let recovered = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
                .unwrap_or_else(|error| panic!("recover two-artifact crash {point}: {error}"));
            let report = recovered
                .committed()
                .expect("two-artifact recovery must return a commit report")
                .clone();
            assert_eq!(report.artifacts().len(), 2);

            let mut reopened =
                open_crash_resource_workspace(&path, recovered.workspace_id().expect("workspace"));
            let finalized = reopened
                .finalize_recovery_at(&locator, &mut AssetLoadBudget::default())
                .unwrap_or_else(|error| panic!("attach two-artifact crash {point}: {error}"));

            assert_eq!(
                finalized,
                RecoveryOutcome::Finalized(Box::new(report.clone()))
            );
            assert_eq!(reopened.revision(), report.committed_revision());
            assert_resource_payload(&reopened, &report);
            assert_eq!(
                AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
                    .expect("redeliver two-artifact report"),
                RecoveryOutcome::HistoricalCommitReceipt(Box::new(report))
            );
        }
    }

    fn assert_target_unchanged(path: &Path, expected: &[u8]) {
        assert_eq!(fs::read(path).expect("target bytes"), expected);
    }

    fn assert_resource_payload(workspace: &AssetWorkspace, report: &CommitReport) {
        let companion = report
            .artifacts()
            .iter()
            .find(|artifact| artifact.source().kind() == SourceKind::StreamedResource)
            .expect("companion artifact")
            .source();
        let range = workspace
            .snapshot()
            .read_source_range(
                companion,
                0,
                u64::try_from(RESOURCE_PAYLOAD.len()).expect("payload length"),
                &mut AssetLoadBudget::default(),
            )
            .expect("read recovered companion");
        let mut payload = Vec::new();
        range.copy_to(&mut payload).expect("copy companion bytes");
        assert_eq!(payload, RESOURCE_PAYLOAD);
    }

    fn remove_terminal_events(report: &CommitReport) {
        let events = report.recovery().root().join("events");
        let mut paths: Vec<_> = fs::read_dir(events)
            .expect("journal events")
            .map(|entry| entry.expect("journal event entry").path())
            .collect();
        paths.sort_unstable();
        assert!(paths.len() >= 2, "fixture must contain terminal events");
        for path in paths.into_iter().rev().take(2) {
            fs::remove_file(path).expect("remove terminal journal event");
        }
    }

    fn truncate_events_after(
        report: &CommitReport,
        retained: fn(&JournalEventKind) -> bool,
    ) -> (JournalLayout, JournalArtifact) {
        let layout = test_layout_from_locator(report.recovery()).expect("journal layout");
        let journal = Journal::open(layout.clone(), &mut AssetLoadBudget::default())
            .expect("open journal for crash simulation");
        let cutoff = journal
            .events()
            .iter()
            .position(|event| retained(event.kind()))
            .expect("retained crash barrier");
        let artifact = journal.manifest().artifacts()[0].clone();
        let mut event_paths: Vec<_> = fs::read_dir(layout.events_directory())
            .expect("journal event directory")
            .map(|entry| entry.expect("journal event entry").path())
            .collect();
        event_paths.sort_unstable();
        let removed: Vec<_> = event_paths.into_iter().skip(cutoff + 1).collect();
        drop(journal);
        for path in removed {
            fs::remove_file(path).expect("truncate event suffix");
        }
        (layout, artifact)
    }

    fn append_recovery_direction(layout: &JournalLayout, direction: RecoveryDirection) {
        let mut journal = Journal::open(layout.clone(), &mut AssetLoadBudget::default())
            .expect("open journal for recovery decision");
        journal
            .append(
                JournalEventKind::RecoveryDecision { direction },
                &mut AssetLoadBudget::default(),
            )
            .expect("append recovery direction");
    }

    fn reopen_base_workspace(workspace: WorkspaceId, path: &Path) -> AssetWorkspace {
        let mut reopened =
            AssetWorkspace::with_workspace_id(workspace, WorkspaceOptions::default())
                .expect("reopened workspace");
        reopened
            .load_source(
                SourceOpenRequest::new(path, SourceAlias::new(SOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load base source");
        reopened
    }

    fn published_restart_fixture() -> (
        TempDir,
        std::path::PathBuf,
        AssetWorkspace,
        CommitReport,
        Vec<u8>,
    ) {
        let (directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let published = fs::read(&path).expect("published target");
        remove_terminal_events(&report);
        drop(workspace);

        fs::write(&path, YAML).expect("restore base target for reopen");
        let reopened = reopen_base_workspace(workspace_id, &path);
        assert_eq!(reopened.revision(), report.base_revision());
        fs::write(&path, &published).expect("restore published target");
        (directory, path, reopened, report, published)
    }

    fn journaled_restart_fixture() -> (
        TempDir,
        std::path::PathBuf,
        AssetWorkspace,
        CommitReport,
        Vec<u8>,
    ) {
        let (directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let published = fs::read(&path).expect("published target");
        let (layout, artifact) =
            truncate_events_after(&report, |kind| matches!(kind, JournalEventKind::Journaled));
        drop(workspace);

        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(&path, &staging, artifact.new_identity())
            .expect("restore staged image identity");
        capture_existing(
            &backup,
            &path,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("restore old target identity");
        let reopened = reopen_base_workspace(workspace_id, &path);
        (directory, path, reopened, report, published)
    }

    fn read_name_at(workspace: &AssetWorkspace, address: &ObjectAddress) -> String {
        let snapshot = workspace.snapshot();
        let mut budget = AssetLoadBudget::default();
        let WorkspaceLookup::Resolved(handle) = snapshot
            .resolve_object(address, &mut budget)
            .expect("resolve recovered object")
        else {
            panic!("recovered object must resolve");
        };
        snapshot
            .read_object(&handle, &mut budget)
            .expect("read recovered object")
            .class()
            .value_at_path(&name_path())
            .expect("recovered name field")
            .as_str()
            .expect("recovered name string")
            .to_owned()
    }

    fn read_name(workspace: &AssetWorkspace) -> String {
        read_name_at(workspace, &address())
    }

    fn existing(
        target: EntryEvidence,
        staging: EntryEvidence,
        backup: EntryEvidence,
    ) -> ArtifactObservation {
        ArtifactObservation {
            target,
            staging,
            backup,
            had_original: true,
        }
    }

    fn absent(target: EntryEvidence, staging: EntryEvidence) -> ArtifactObservation {
        ArtifactObservation {
            target,
            staging,
            backup: EntryEvidence::Missing,
            had_original: false,
        }
    }

    fn observation(artifacts: Vec<ArtifactObservation>) -> RecoveryObservation {
        RecoveryObservation {
            events: EventFacts {
                artifacts: vec![ArtifactEventFacts::default(); artifacts.len()],
                ..EventFacts::default()
            },
            artifacts,
            baseline: BaselineObservation::Base,
        }
    }

    #[test]
    fn rollback_direction_rejects_later_forward_events() {
        let rollback = JournalEvent::new(
            0,
            None,
            JournalEventKind::RecoveryDecision {
                direction: RecoveryDirection::Rollback,
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let staging = JournalEvent::new(
            1,
            Some(rollback.digest()),
            JournalEventKind::StagingVerified,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut facts = EventFacts::default();
        facts.apply(&rollback, &[], &[]).unwrap();

        assert!(matches!(
            facts.apply(&staging, &[], &[]),
            Err(RecoveryBlockedReason::InvalidEventSequence { .. })
        ));
    }

    #[test]
    fn published_transaction_rejects_late_rollback_decision() {
        let rollback = JournalEvent::new(
            0,
            None,
            JournalEventKind::RecoveryDecision {
                direction: RecoveryDirection::Rollback,
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut facts = EventFacts {
            published: true,
            ..EventFacts::default()
        };

        assert!(matches!(
            facts.apply(&rollback, &[], &[]),
            Err(RecoveryBlockedReason::InvalidEventSequence { .. })
        ));
    }

    #[test]
    fn complete_staging_prefers_forward() {
        let state = observation(vec![
            existing(
                EntryEvidence::Old,
                EntryEvidence::New,
                EntryEvidence::Missing,
            ),
            absent(EntryEvidence::Missing, EntryEvidence::New),
        ]);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Forward
        );
    }

    #[test]
    fn partial_promotion_still_prefers_forward() {
        let state = observation(vec![
            existing(
                EntryEvidence::New,
                EntryEvidence::Missing,
                EntryEvidence::Old,
            ),
            absent(EntryEvidence::Missing, EntryEvidence::New),
        ]);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Forward
        );
    }

    #[test]
    fn missing_unpromoted_stage_forces_rollback() {
        let state = observation(vec![
            existing(
                EntryEvidence::New,
                EntryEvidence::Missing,
                EntryEvidence::Old,
            ),
            absent(EntryEvidence::Missing, EntryEvidence::Missing),
        ]);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Rollback
        );
    }

    #[test]
    fn persisted_rollback_direction_is_sticky() {
        let mut state = observation(vec![existing(
            EntryEvidence::New,
            EntryEvidence::Missing,
            EntryEvidence::Old,
        )]);
        state.events.direction = Some(RecoveryDirection::Rollback);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Rollback
        );
    }

    #[test]
    fn unexpected_bytes_block_both_directions() {
        let state = observation(vec![existing(
            EntryEvidence::Unexpected,
            EntryEvidence::New,
            EntryEvidence::Missing,
        )]);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Blocked
        );
    }

    #[test]
    fn corrupt_staging_before_target_mutation_is_not_recoverable() {
        let state = observation(vec![existing(
            EntryEvidence::Old,
            EntryEvidence::CorruptNew,
            EntryEvidence::Missing,
        )]);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Blocked
        );
    }

    #[test]
    fn corrupt_owned_target_before_publication_forces_rollback() {
        let state = observation(vec![existing(
            EntryEvidence::CorruptNew,
            EntryEvidence::Missing,
            EntryEvidence::Old,
        )]);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Rollback
        );
    }

    #[test]
    fn corrupt_captured_backup_is_restored_before_blocking() {
        let state = observation(vec![existing(
            EntryEvidence::Missing,
            EntryEvidence::New,
            EntryEvidence::CorruptOld,
        )]);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Rollback
        );
    }

    #[test]
    fn published_bytes_with_base_revision_choose_forward_rebuild() {
        let mut state = observation(vec![existing(
            EntryEvidence::New,
            EntryEvidence::Missing,
            EntryEvidence::Old,
        )]);
        state.events.published = true;
        state.events.artifacts[0].promoted = true;
        state.baseline = BaselineObservation::Base;
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Forward
        );
    }

    #[test]
    fn finalized_commit_redelivers_only_with_committed_baseline() {
        let mut state = observation(vec![absent(EntryEvidence::New, EntryEvidence::Missing)]);
        state.events.published = true;
        state.events.baseline_installed = true;
        state.events.finalized = true;
        state.events.artifacts[0].promoted = true;
        state.baseline = BaselineObservation::Committed;
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Forward
        );

        state.baseline = BaselineObservation::Base;
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Forward
        );
    }

    #[test]
    fn promoted_existing_target_without_backup_is_blocked() {
        let state = observation(vec![existing(
            EntryEvidence::New,
            EntryEvidence::Missing,
            EntryEvidence::Missing,
        )]);
        assert_eq!(
            decide_recovery(&state).disposition,
            RecoveryDisposition::Blocked
        );
    }

    #[test]
    fn finalized_recovery_redelivers_the_same_report_idempotently() {
        let (_directory, _path, mut workspace, report) = committed_fixture();
        let first = workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("finalized recovery");
        assert_eq!(first, RecoveryOutcome::Finalized(Box::new(report.clone())));

        let second = workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("idempotent finalized recovery");
        assert_eq!(second, first);
    }

    #[test]
    fn finalized_workspace_mismatch_does_not_poison_canonical_redelivery() {
        let (_directory, _path, mut workspace, report) = committed_fixture();
        let events = report.recovery().root().join("events");
        let event_count = fs::read_dir(&events).expect("events").count();
        let mut wrong_workspace = AssetWorkspace::new().expect("wrong workspace");

        let error = wrong_workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("workspace mismatch must block recovery");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::WorkspaceMismatch { .. })
        ));
        assert_eq!(fs::read_dir(&events).expect("events").count(), event_count);

        let outcome = workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("canonical recovery must remain available");
        assert_eq!(outcome, RecoveryOutcome::Finalized(Box::new(report)));
    }

    #[test]
    fn published_recovery_rebuilds_and_installs_the_committed_baseline() {
        let (_directory, _path, mut reopened, report, _published) = published_restart_fixture();

        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("recover published transaction");

        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(reopened.revision(), report.committed_revision());
        assert_eq!(read_name(&reopened), "After");
    }

    #[test]
    fn published_recovery_obeys_exact_and_one_short_budgets_without_writing() {
        let (_measured_directory, _measured_path, mut measured_workspace, measured_report, _) =
            published_restart_fixture();
        let mut measured = AssetLoadBudget::default();
        measured_workspace
            .finalize_recovery_at(measured_report.recovery(), &mut measured)
            .expect("measure published recovery");
        let usage = measured.usage();
        let exact_limits = unity_asset_core::AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..unity_asset_core::AssetLoadLimits::default()
        };

        let (_exact_directory, _exact_path, mut exact_workspace, exact_report, _) =
            published_restart_fixture();
        let mut exact = AssetLoadBudget::new(exact_limits).expect("exact budget");
        exact_workspace
            .finalize_recovery_at(exact_report.recovery(), &mut exact)
            .expect("exact recovery budget");
        assert_eq!(exact.usage(), usage);

        let (_short_directory, short_path, mut short_workspace, short_report, published) =
            published_restart_fixture();
        let events = short_report.recovery().root().join("events");
        let event_count = fs::read_dir(&events).expect("events").count();
        let mut one_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .expect("one-short budget");

        let error = short_workspace
            .finalize_recovery_at(short_report.recovery(), &mut one_short)
            .expect_err("one-short recovery must fail");
        assert!(matches!(error, RecoveryError::Budget { .. }));
        assert_eq!(short_workspace.revision(), short_report.base_revision());
        assert_eq!(fs::read(short_path).expect("published target"), published);
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);
    }

    #[test]
    fn preopen_recovery_precharges_exact_budget_before_any_durable_write() {
        let (_measured_directory, _measured_path, _measured_workspace, measured_report, _) =
            journaled_restart_fixture();
        let mut measured = AssetLoadBudget::default();
        AssetWorkspace::recover_at(measured_report.recovery(), &mut measured)
            .expect("measure journaled recovery");
        let usage = measured.usage();
        let exact_limits = unity_asset_core::AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..unity_asset_core::AssetLoadLimits::default()
        };

        let (_directory, path, mut workspace, report, published) = journaled_restart_fixture();
        let layout = test_layout_from_locator(report.recovery()).expect("journal layout");
        let journal = Journal::open(layout.clone(), &mut AssetLoadBudget::default())
            .expect("journal before one-short recovery");
        let artifact = journal.manifest().artifacts()[0].clone();
        drop(journal);
        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        let target_before = fs::read(&path).expect("target before recovery");
        let staging_before = fs::read(&staging).expect("staging before recovery");
        let events = layout.events_directory();
        let event_count = fs::read_dir(events).expect("events").count();
        assert!(!backup.exists());
        let mut one_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .expect("one-short budget");

        let error = AssetWorkspace::recover_at(report.recovery(), &mut one_short)
            .expect_err("one-short journaled recovery must fail before mutation");

        assert!(matches!(error, RecoveryError::Budget { .. }));
        assert_eq!(workspace.revision(), report.base_revision());
        assert_eq!(fs::read(&path).expect("unchanged target"), target_before);
        assert_eq!(
            fs::read(&staging).expect("unchanged staging"),
            staging_before
        );
        assert!(!backup.exists());
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);

        let mut exact = AssetLoadBudget::new(exact_limits).expect("exact budget");
        let recovered = AssetWorkspace::recover_at(report.recovery(), &mut exact)
            .expect("exact recovery after one-short probe");
        assert_eq!(
            recovered,
            RecoveryOutcome::FilesystemRecovered(Box::new(report.clone()))
        );
        assert_eq!(exact.usage(), usage);
        assert_eq!(fs::read(path).expect("published target"), published);
        let finalized = workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("attach exact preopen recovery");
        assert_eq!(
            finalized,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(workspace.revision(), report.committed_revision());
    }

    #[test]
    fn journaled_recovery_captures_the_old_target_and_publishes_staging() {
        let (directory, path, mut reopened, report, published) = journaled_restart_fixture();
        let locator = PublicationTarget::in_place(directory.path())
            .expect("publication target")
            .recovery_locator(report.transaction());
        assert_eq!(&locator, report.recovery());

        let recovered = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("recover journaled filesystem transaction");
        let outcome = reopened
            .finalize_recovery_at(&locator, &mut AssetLoadBudget::default())
            .expect("attach journaled transaction");

        assert!(recovered.requires_workspace_finalization());
        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(recovered.committed(), outcome.committed());
        assert_eq!(fs::read(&path).expect("recovered target"), published);
        assert_eq!(reopened.revision(), report.committed_revision());
        assert_eq!(read_name(&reopened), "After");
    }

    #[test]
    fn explicit_abandon_rolls_back_an_unpublished_journaled_transaction() {
        let (_directory, path, reopened, report, _published) = journaled_restart_fixture();

        let outcome =
            AssetWorkspace::abandon_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("explicit abandon must roll back journaled evidence");

        assert_eq!(outcome, rollback_outcome(&report));
        assert_eq!(fs::read(&path).expect("restored target"), YAML);
        assert_eq!(reopened.revision(), report.base_revision());

        let layout = test_layout_from_locator(report.recovery()).expect("journal layout");
        let journal =
            Journal::open(layout, &mut AssetLoadBudget::default()).expect("open abandoned journal");
        assert!(journal.events().iter().any(|event| {
            matches!(
                event.kind(),
                JournalEventKind::RecoveryDecision {
                    direction: RecoveryDirection::Rollback
                }
            )
        }));
        assert!(
            journal
                .events()
                .iter()
                .any(|event| matches!(event.kind(), JournalEventKind::Abandoned))
        );
        assert!(
            journal
                .events()
                .iter()
                .any(|event| matches!(event.kind(), JournalEventKind::Finalized))
        );
    }

    #[test]
    fn finalized_rollback_redelivers_historical_receipt_after_later_target_tampering() {
        let (_directory, path, _workspace, report, _published) = journaled_restart_fixture();
        let receipt =
            AssetWorkspace::abandon_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("finalize rollback");
        assert!(receipt.rolled_back().is_some());

        fs::write(&path, b"external replacement").expect("tamper rolled-back target");
        let redelivered =
            AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("finalized rollback receipt remains historical evidence");

        assert_eq!(
            redelivered,
            RecoveryOutcome::HistoricalRollbackReceipt(
                receipt
                    .rolled_back()
                    .expect("initial rollback is current")
                    .clone(),
            )
        );
        assert!(redelivered.rolled_back().is_none());
        assert!(redelivered.historical_rollback_receipt().is_some());
        assert_eq!(
            fs::read(path).expect("tampered target remains"),
            b"external replacement"
        );
    }

    #[test]
    fn explicit_abandon_refuses_a_sticky_forward_transaction() {
        let (_directory, path, mut reopened, report, published) = journaled_restart_fixture();
        let layout = test_layout_from_locator(report.recovery()).expect("journal layout");
        append_recovery_direction(&layout, RecoveryDirection::Forward);
        let original = fs::read(&path).expect("base target");
        let events = layout.events_directory();
        let event_count = fs::read_dir(events).expect("events").count();

        let error = AssetWorkspace::abandon_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("sticky forward transaction cannot be abandoned");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::InvalidEventSequence { .. })
        ));
        assert_eq!(fs::read(&path).expect("unchanged target"), original);
        assert_eq!(reopened.revision(), report.base_revision());
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);

        let recovered =
            AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("ordinary preopen recovery remains available");
        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("attach ordinary forward recovery");
        assert!(recovered.requires_workspace_finalization());
        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(recovered.committed(), outcome.committed());
        assert_eq!(fs::read(&path).expect("published target"), published);
    }

    #[test]
    fn wrong_recovery_context_does_not_poison_an_unfinished_journal() {
        let (_directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let published = fs::read(&path).expect("published target");
        let (layout, artifact) =
            truncate_events_after(&report, |kind| matches!(kind, JournalEventKind::Journaled));
        drop(workspace);

        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(&path, &staging, artifact.new_identity())
            .expect("restore staged image identity");
        capture_existing(
            &backup,
            &path,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("restore old target identity");
        let events = layout.events_directory();
        let event_count = fs::read_dir(events).expect("events").count();

        let mut wrong_workspace = AssetWorkspace::new().expect("wrong workspace");
        let error = wrong_workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("wrong workspace must block recovery");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::WorkspaceMismatch { .. })
        ));
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);

        let mut wrong_revision = reopen_base_workspace(workspace_id, &path);
        for alias in ["unrelated-a.prefab", "unrelated-b.prefab"] {
            let extra = path.with_file_name(alias);
            fs::write(&extra, YAML).expect("unrelated fixture");
            wrong_revision
                .load_source(
                    SourceOpenRequest::new(&extra, SourceAlias::new(alias).expect("alias"))
                        .with_kind_hint(SourceKind::Yaml),
                    &mut AssetLoadBudget::default(),
                )
                .expect("advance unrelated revision");
        }
        assert_ne!(wrong_revision.revision(), report.base_revision());
        assert_ne!(wrong_revision.revision(), report.committed_revision());
        let error = wrong_revision
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("attachment must require preopen recovery");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::FilesystemRecoveryRequired)
        ));
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);
        assert_eq!(fs::read(&path).expect("base target"), YAML);

        AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("preopen recovery with no workspace context");
        let recovered_event_count = fs::read_dir(events).expect("events").count();
        let error = wrong_revision
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("unrelated loaded sources must block baseline attachment");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::BaselineUnavailable { .. })
        ));
        assert_eq!(
            fs::read_dir(events).expect("events").count(),
            recovered_event_count
        );
        assert_eq!(fs::read(&path).expect("published target"), published);

        let mut reopened = reopen_base_workspace(workspace_id, &path);
        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("correct recovery remains available");
        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(fs::read(&path).expect("recovered target"), published);
    }

    #[test]
    fn same_identity_stage_tamper_is_blocked_before_target_mutation() {
        let (_directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let (layout, artifact) =
            truncate_events_after(&report, |kind| matches!(kind, JournalEventKind::Journaled));
        drop(workspace);

        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(&path, &staging, artifact.new_identity())
            .expect("restore staged image identity");
        capture_existing(
            &backup,
            &path,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("restore old target identity");

        let mut tampered = fs::read(&staging).expect("staged bytes");
        tampered[0] ^= 1;
        fs::write(&staging, &tampered).expect("same-inode stage tamper");
        let tampered_identity = observe_file_identity(&staging).expect("tampered stage identity");
        assert_eq!(&tampered_identity, artifact.new_identity());

        let reopened = reopen_base_workspace(workspace_id, &path);
        let error = AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("tampered staging must block recovery");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(fs::read(&path).expect("unchanged target"), YAML);
        assert_eq!(reopened.revision(), report.base_revision());
    }

    #[test]
    fn same_identity_target_tamper_before_published_is_rolled_back_privately() {
        let (_directory, path, workspace, report, published) = published_restart_fixture();
        let (layout, artifact) = truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::PromotionIntent { .. })
        });
        let staging = artifact.staging().join_root(layout.directory());

        let mut tampered = published;
        tampered[0] ^= 1;
        fs::write(&path, &tampered).expect("same-inode target tamper");
        assert_eq!(
            &observe_file_identity(&path).expect("tampered target identity"),
            artifact.new_identity()
        );

        let outcome =
            AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("owned corrupt target can be rolled back");

        assert_eq!(outcome, rollback_outcome(&report));
        assert_eq!(fs::read(&path).expect("restored target"), YAML);
        assert_eq!(
            fs::read(staging).expect("preserved corrupt image"),
            tampered
        );
        assert_eq!(workspace.revision(), report.base_revision());
    }

    #[test]
    fn sticky_forward_rehomes_corrupt_new_inode_and_restores_old_target() {
        let (_directory, path, workspace, report, published) = published_restart_fixture();
        let (layout, artifact) = truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::PromotionIntent { .. })
        });
        append_recovery_direction(&layout, RecoveryDirection::Forward);
        let staging = artifact.staging().join_root(layout.directory());

        let mut tampered = published;
        tampered[0] ^= 1;
        fs::write(&path, &tampered).expect("same-inode target tamper");

        let error = AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("sticky forward cannot publish corrupt owned bytes");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(fs::read(&path).expect("restored target"), YAML);
        assert_eq!(
            fs::read(staging).expect("preserved corrupt image"),
            tampered
        );
        assert_eq!(workspace.revision(), report.base_revision());
    }

    #[test]
    fn same_identity_backup_tamper_is_restored_to_target_before_blocking() {
        let (_directory, path, workspace, report, published) = published_restart_fixture();
        let (layout, artifact) = truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::BackupIntent { .. })
        });
        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(&path, &staging, artifact.new_identity())
            .expect("restore staged image identity");

        let mut tampered = fs::read(&backup).expect("captured old image");
        tampered[0] ^= 1;
        fs::write(&backup, &tampered).expect("same-inode backup tamper");
        assert_eq!(
            &observe_file_identity(&backup).expect("tampered backup identity"),
            artifact.old_identity().expect("old target identity")
        );

        let error = AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("external writes to the captured old inode must remain visible");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(fs::read(&path).expect("restored external bytes"), tampered);
        assert!(!backup.exists());
        assert_eq!(
            fs::read(staging).expect("preserved staged image"),
            published
        );
        assert_eq!(workspace.revision(), report.base_revision());
    }

    #[test]
    fn sticky_forward_restores_corrupt_old_inode_to_its_external_path() {
        let (_directory, path, workspace, report, published) = published_restart_fixture();
        let (layout, artifact) = truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::BackupIntent { .. })
        });
        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(&path, &staging, artifact.new_identity())
            .expect("restore staged image identity");
        append_recovery_direction(&layout, RecoveryDirection::Forward);

        let mut tampered = fs::read(&backup).expect("captured old image");
        tampered[0] ^= 1;
        fs::write(&backup, &tampered).expect("same-inode backup tamper");

        let error = AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("sticky forward cannot strand external old bytes in the journal");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(fs::read(&path).expect("restored external bytes"), tampered);
        assert!(!backup.exists());
        assert_eq!(
            fs::read(staging).expect("preserved staged image"),
            published
        );
        assert_eq!(workspace.revision(), report.base_revision());
    }

    #[test]
    fn byte_identical_target_replacement_after_journal_is_never_captured() {
        let (_directory, path, workspace, report) = committed_fixture();
        let (layout, artifact) =
            truncate_events_after(&report, |kind| matches!(kind, JournalEventKind::Journaled));
        drop(workspace);

        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(&path, &staging, artifact.new_identity())
            .expect("restore staged image identity");
        capture_existing(
            &backup,
            &path,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("restore base target identity");
        fs::remove_file(&path).expect("remove original target");
        fs::write(&path, YAML).expect("byte-identical replacement");
        let replacement_identity = observe_file_identity(&path).expect("replacement identity");
        assert_ne!(
            &replacement_identity,
            artifact.old_identity().expect("existing target identity")
        );

        let error = AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("replacement identity must block recovery");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(fs::read(&path).expect("replacement remains"), YAML);
        assert_eq!(
            observe_file_identity(&path).expect("replacement identity remains"),
            replacement_identity
        );
        assert!(staging.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn recovery_finishes_a_backup_rename_missing_its_completion_event() {
        let (_directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let published = fs::read(&path).expect("published target");
        let (layout, artifact) = truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::BackupIntent { .. })
        });
        drop(workspace);

        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(&path, &staging, artifact.new_identity())
            .expect("restore staged image identity");
        capture_existing(
            &backup,
            &path,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("restore base target identity");
        capture_existing(
            &path,
            &backup,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("simulate completed backup rename");

        let recovered =
            AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("recover captured backup");
        let mut reopened = reopen_base_workspace(workspace_id, &path);
        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("attach recovered backup transaction");

        assert!(recovered.requires_workspace_finalization());
        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(recovered.committed(), outcome.committed());
        assert_eq!(fs::read(&path).expect("recovered target"), published);
        assert_eq!(reopened.revision(), report.committed_revision());
    }

    #[test]
    fn recovery_finishes_a_promotion_rename_missing_its_completion_event() {
        let (_directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let published = fs::read(&path).expect("published target");
        truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::PromotionIntent { .. })
        });
        drop(workspace);

        fs::write(&path, YAML).expect("restore old target before promotion");
        fs::write(&path, &published).expect("simulate completed promotion rename");

        let recovered =
            AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("recover promoted target");
        let mut reopened = reopen_base_workspace(workspace_id, &path);
        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("attach promoted transaction");

        assert!(recovered.requires_workspace_finalization());
        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(recovered.committed(), outcome.committed());
        assert_eq!(fs::read(&path).expect("recovered target"), published);
        assert_eq!(reopened.revision(), report.committed_revision());
    }

    #[test]
    fn published_recovery_recreates_a_new_companion_source() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(RESOURCE_ALIAS);
        fs::write(&path, RESOURCE_YAML).expect("resource fixture");
        let mut workspace = AssetWorkspace::new().expect("workspace");
        workspace
            .load_source(
                SourceOpenRequest::new(&path, SourceAlias::new(RESOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load resource fixture");
        let workspace_id = workspace.workspace_id();
        let prepared = workspace
            .prepare(
                resource_plan(&workspace),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare resource mutation");
        let report = workspace
            .commit(
                prepared,
                PublicationTarget::in_place(directory.path()).expect("publication target"),
                &mut AssetLoadBudget::default(),
            )
            .expect("commit resource mutation");
        let companion = report
            .artifacts()
            .iter()
            .find(|artifact| artifact.source().kind() == SourceKind::StreamedResource)
            .expect("companion artifact")
            .source();
        let published = fs::read(&path).expect("published YAML");
        remove_terminal_events(&report);
        drop(workspace);

        fs::write(&path, RESOURCE_YAML).expect("restore base YAML");
        let mut reopened =
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default())
                .expect("reopened workspace");
        reopened
            .load_source(
                SourceOpenRequest::new(&path, SourceAlias::new(RESOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load base YAML");
        assert_eq!(reopened.revision(), report.base_revision());
        fs::write(&path, published).expect("restore published YAML");

        reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("recover resource transaction");

        assert_eq!(reopened.revision(), report.committed_revision());
        let range = reopened
            .snapshot()
            .read_source_range(
                companion,
                0,
                u64::try_from(RESOURCE_PAYLOAD.len()).expect("payload length"),
                &mut AssetLoadBudget::default(),
            )
            .expect("read recovered companion");
        let mut payload = Vec::new();
        range.copy_to(&mut payload).expect("copy companion bytes");
        assert_eq!(payload, RESOURCE_PAYLOAD);
    }

    #[test]
    fn finalized_receipt_redelivers_historical_commit_without_replacing_an_external_target() {
        let (_directory, path, mut workspace, report) = committed_fixture();
        let events = report.recovery().root().join("events");
        let event_count = fs::read_dir(&events).expect("events").count();
        let tampered = b"externally replaced bytes";
        fs::write(&path, tampered).expect("tamper target");

        let outcome = workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("finalized receipt remains available after successor bytes");
        assert_eq!(
            outcome,
            RecoveryOutcome::HistoricalCommitReceipt(Box::new(report))
        );
        assert!(outcome.finalized().is_none());
        assert!(outcome.historical_commit_receipt().is_some());
        assert_target_unchanged(&path, tampered);
        assert_eq!(fs::read_dir(&events).expect("events").count(), event_count);
    }

    #[test]
    fn finalized_receipt_redelivers_without_replacing_a_successor_commit() {
        let (directory, path, mut workspace, first) = committed_fixture();
        let second_prepared = workspace
            .prepare(
                mutation_plan(&workspace, "After", "Later"),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare successor mutation");
        let second = workspace
            .commit(
                second_prepared,
                PublicationTarget::in_place(directory.path()).expect("publication target"),
                &mut AssetLoadBudget::default(),
            )
            .expect("commit successor mutation");
        let successor_bytes = fs::read(&path).expect("successor target");

        let detached =
            AssetWorkspace::recover_at(first.recovery(), &mut AssetLoadBudget::default())
                .expect("redeliver first filesystem receipt");
        assert_eq!(
            detached,
            RecoveryOutcome::HistoricalCommitReceipt(Box::new(first.clone()))
        );
        assert!(detached.filesystem_recovered().is_none());
        assert!(detached.historical_commit_receipt().is_some());
        assert_target_unchanged(&path, &successor_bytes);

        let attached = workspace
            .finalize_recovery_at(first.recovery(), &mut AssetLoadBudget::default())
            .expect("redeliver first finalized receipt");
        assert_eq!(
            attached,
            RecoveryOutcome::HistoricalCommitReceipt(Box::new(first))
        );
        assert!(attached.finalized().is_none());
        assert!(attached.historical_commit_receipt().is_some());
        assert_eq!(workspace.revision(), second.committed_revision());
        assert_target_unchanged(&path, &successor_bytes);
    }

    #[test]
    fn recovery_discovery_wire_requires_the_current_version_and_shape() {
        let discovery = RecoveryDiscovery::new(Vec::new());
        let encoded = serde_json::to_value(&discovery).expect("serialize discovery response");
        assert_eq!(
            encoded,
            serde_json::json!({
                "version": RECOVERY_DISCOVERY_VERSION,
                "recoveries": [],
            })
        );
        let decoded = serde_json::from_value::<RecoveryDiscovery>(encoded.clone())
            .expect("deserialize current discovery response");
        assert_eq!(decoded, discovery);

        let unsupported = serde_json::json!({
            "version": RECOVERY_DISCOVERY_VERSION + 1,
            "recoveries": [],
        });
        assert!(serde_json::from_value::<RecoveryDiscovery>(unsupported).is_err());

        let unknown_field = serde_json::json!({
            "version": RECOVERY_DISCOVERY_VERSION,
            "recoveries": [],
            "unexpected": true,
        });
        assert!(serde_json::from_value::<RecoveryDiscovery>(unknown_field).is_err());
    }

    #[test]
    fn recovery_discovery_reports_a_busy_publication_guard() {
        let (directory, _path, _workspace, _report) = committed_fixture();
        let target = PublicationTarget::in_place(directory.path()).expect("publication target");
        let _guard = CommitGuard::acquire(directory.path()).expect("publication guard");

        let error = target
            .discover_recoveries(&mut AssetLoadBudget::default())
            .expect_err("discovery must not race a publication");
        assert!(matches!(error, RecoveryDiscoveryError::Busy));
    }

    #[test]
    fn noncanonical_locator_is_blocked_before_any_target_write() {
        let (_directory, path, mut workspace, report) = committed_fixture();
        let original = fs::read(&path).expect("committed target");
        let malicious = RecoveryLocator::new(
            report.recovery().root().join("..").join("escape"),
            report.transaction(),
            report.recovery().root_identity().clone(),
        );

        let error = workspace
            .finalize_recovery_at(&malicious, &mut AssetLoadBudget::default())
            .expect_err("noncanonical locator must block");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::InvalidLocator { .. })
        ));
        assert_target_unchanged(&path, &original);
    }

    #[test]
    fn replaced_publication_root_is_blocked_before_recovery_namespace_creation() {
        let parent = tempfile::tempdir().expect("temporary publication parent");
        let root = parent.path().join("publication-root");
        let original = parent.path().join("original-publication-root");
        std::fs::create_dir(&root).expect("publication root");
        let target = PublicationTarget::in_place(&root).expect("publication target");
        let transaction = TransactionId::new(DigestV1::hash_bytes(b"replaced recovery root"));
        let locator = target.recovery_locator(transaction);

        std::fs::rename(&root, &original).expect("move original publication root");
        std::fs::create_dir(&root).expect("replacement publication root");

        let error = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect_err("replacement publication root must be blocked");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::InvalidLocator { .. })
        ));
        assert!(!root.join(RECOVERY_DIRECTORY).exists());
        assert!(!original.join(RECOVERY_DIRECTORY).exists());
    }

    #[cfg(unix)]
    #[test]
    fn root_replacement_after_recovery_guard_preserves_old_evidence_and_skips_replacement() {
        let parent = tempfile::tempdir().expect("temporary publication parent");
        let root = parent.path().join("publication-root");
        let original = parent.path().join("original-publication-root");
        fs::create_dir(&root).expect("publication root");
        let path = root.join(SOURCE_ALIAS);
        fs::write(&path, YAML).expect("fixture bytes");
        let locator = crash_locator(&root, &path);
        run_crash_child(&root, "recovery_baseline_synced", None);
        let layout = test_layout_from_locator(&locator).expect("recovery layout");
        let preparation_name = layout
            .preparation_path()
            .file_name()
            .expect("preparation filename")
            .to_os_string();
        let transaction_name = layout
            .directory()
            .file_name()
            .expect("transaction filename")
            .to_os_string();
        let replacement = root.clone();
        let moved = original.clone();
        test_install_recovery_post_guard_hook(locator.root().to_path_buf(), move || {
            fs::rename(&replacement, &moved).expect("move original publication root");
            fs::create_dir(&replacement).expect("replacement publication root");
        });

        let error = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect_err("root replacement after guard must block recovery");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::InvalidLocator { .. })
        ));
        assert!(!root.join(RECOVERY_DIRECTORY).exists());
        let original_v2 = original
            .join(RECOVERY_DIRECTORY)
            .join(RECOVERY_VERSION_DIRECTORY);
        assert!(original_v2.join(preparation_name).is_file());
        assert!(original_v2.join(transaction_name).is_dir());
    }
}
