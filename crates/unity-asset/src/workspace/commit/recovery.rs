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

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, TransactionId, WorkspaceId, WorkspaceRevision,
    vec_allocation_bytes,
};

use super::super::portable_path::{PortablePathError, slash_key};

use super::super::WorkspaceInstallationDigest;
use super::journal::{
    BACKUP_DIRECTORY, BASELINE_DIRECTORY, EVENTS_DIRECTORY, Journal, JournalArtifact,
    JournalBaselineImage, JournalError, JournalEvent, JournalEventKind, JournalEventPlan,
    JournalLayout, JournalPath, JournalPreparation, MANIFEST_TEMPORARY_FILE,
    OpenedJournalPreparation, PlannedJournalEvent, RECOVERY_DIRECTORY, RECOVERY_VERSION_DIRECTORY,
    RecoveryEvidenceName, STAGE_DIRECTORY, matches_ordinal_journal_path,
    parse_recovery_evidence_name,
};
use super::platform::{
    COMMIT_LOCK_FILE, CommitGuard, CommitLockPathError, CommitLockPaths,
    DIRECTORY_VISIT_ENTRY_BYTES, DIRECTORY_VISIT_SETUP_BYTES, DirectoryEntryName,
    DirectoryIdentity, DirectoryVisitError, FileIdentity, JournalAccess, JournalDirectory,
    LEGACY_COMMIT_LOCK_DIRECTORY, SecurityMetadataCopyReservation, SecurityMetadataError,
    capture_external_regular_in_journal_directory, capture_journal_regular,
    copy_security_metadata_between_journal_directories, journal_access, journal_directory_identity,
    observe_directory_identity, open_commit_root, open_existing_journal_namespace,
    open_journal_directory, open_journal_directory_in_directory, open_journal_regular,
    open_journal_regular_in_directory, open_readonly_regular_in_parent, opened_file_identity,
    promote_journal_regular_to_external, remove_journal_directory,
    remove_journal_directory_in_directory, remove_journal_regular,
    remove_journal_regular_in_directory, reserve_security_metadata_copy, sync_journal_access,
    visit_existing_directory_entries, visit_journal_directory_entries,
};
#[cfg(test)]
use super::platform::{capture_existing, observe_file_identity};
#[cfg(all(test, any(unix, windows)))]
use super::platform::{test_security_metadata_matches, test_tamper_security_metadata};
use super::publication_protocol::{
    ArtifactObservation, ArtifactProgress, BaselineObservation, EntryEvidence, PreparedTransition,
    ProtocolBlock, ProtocolError, ProtocolEvent, ProtocolPlanError, PublicationAction,
    PublicationState, RecoveryDecision, RecoveryDirection, RecoveryIntent, RecoveryRequest,
    RecoveryStep, append_recovery_program, decide_recovery,
};
use super::{AssetWorkspace, CommitReport, PublicationTarget, RecoveryLocator, VerificationCharge};

/// Version of the rollback-receipt response contract.
pub const ROLLBACK_RECEIPT_VERSION: u8 = 3;

/// Stable receipt for a transaction whose pre-publication state was restored.
///
/// A deserialized receipt is historical evidence only. Recovery remains an
/// explicit operation against a caller-authorized [`RecoveryLocator`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackReceipt {
    version: u8,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    base_installation: WorkspaceInstallationDigest,
    recovery: RecoveryLocator,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackReceiptWire {
    version: u8,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    base_installation: WorkspaceInstallationDigest,
    recovery: RecoveryLocator,
}

impl<'de> Deserialize<'de> for RollbackReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RollbackReceiptWire::deserialize(deserializer)?;
        let receipt = Self {
            version: wire.version,
            workspace_id: wire.workspace_id,
            base_revision: wire.base_revision,
            base_installation: wire.base_installation,
            recovery: wire.recovery,
        };
        receipt.validate().map_err(serde::de::Error::custom)?;
        Ok(receipt)
    }
}

impl RollbackReceipt {
    const fn new(
        workspace_id: WorkspaceId,
        base_revision: WorkspaceRevision,
        base_installation: WorkspaceInstallationDigest,
        recovery: RecoveryLocator,
    ) -> Self {
        Self {
            version: ROLLBACK_RECEIPT_VERSION,
            workspace_id,
            base_revision,
            base_installation,
            recovery,
        }
    }

    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
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
    pub const fn base_installation(&self) -> WorkspaceInstallationDigest {
        self.base_installation
    }

    #[must_use]
    pub const fn recovery(&self) -> &RecoveryLocator {
        &self.recovery
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.version != ROLLBACK_RECEIPT_VERSION {
            return Err("unsupported rollback receipt version");
        }
        Ok(())
    }
}

/// Version of the terminal recovery-outcome response contract.
pub const RECOVERY_OUTCOME_VERSION: u8 = 3;

/// Terminal result of recovering one transaction.
///
/// Serialized outcomes retain the live operation's exact status. Deserialization
/// deliberately downgrades current-state assertions to historical receipts:
/// untrusted JSON can describe evidence, but it cannot authorize recovery or
/// assert that a filesystem or workspace is still current.
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

#[derive(Serialize)]
struct RecoveryOutcomeRef<'value> {
    version: u8,
    outcome: RecoveryOutcomeRefKind<'value>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum RecoveryOutcomeRefKind<'value> {
    FilesystemRecovered { report: &'value CommitReport },
    Finalized { report: &'value CommitReport },
    HistoricalCommitReceipt { report: &'value CommitReport },
    RolledBack { receipt: &'value RollbackReceipt },
    HistoricalRollbackReceipt { receipt: &'value RollbackReceipt },
    NoTransaction { recovery: &'value RecoveryLocator },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryOutcomeWire {
    version: u8,
    outcome: RecoveryOutcomeWireKind,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum RecoveryOutcomeWireKind {
    FilesystemRecovered { report: Box<CommitReport> },
    Finalized { report: Box<CommitReport> },
    HistoricalCommitReceipt { report: Box<CommitReport> },
    RolledBack { receipt: RollbackReceipt },
    HistoricalRollbackReceipt { receipt: RollbackReceipt },
    NoTransaction { recovery: RecoveryLocator },
}

impl Serialize for RecoveryOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let outcome = match self {
            Self::FilesystemRecovered(report) => {
                RecoveryOutcomeRefKind::FilesystemRecovered { report }
            }
            Self::Finalized(report) => RecoveryOutcomeRefKind::Finalized { report },
            Self::HistoricalCommitReceipt(report) => {
                RecoveryOutcomeRefKind::HistoricalCommitReceipt { report }
            }
            Self::RolledBack(receipt) => RecoveryOutcomeRefKind::RolledBack { receipt },
            Self::HistoricalRollbackReceipt(receipt) => {
                RecoveryOutcomeRefKind::HistoricalRollbackReceipt { receipt }
            }
            Self::NoTransaction(recovery) => RecoveryOutcomeRefKind::NoTransaction { recovery },
        };
        RecoveryOutcomeRef {
            version: RECOVERY_OUTCOME_VERSION,
            outcome,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RecoveryOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RecoveryOutcomeWire::deserialize(deserializer)?;
        if wire.version != RECOVERY_OUTCOME_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported recovery outcome version",
            ));
        }
        Ok(match wire.outcome {
            RecoveryOutcomeWireKind::FilesystemRecovered { report }
            | RecoveryOutcomeWireKind::Finalized { report }
            | RecoveryOutcomeWireKind::HistoricalCommitReceipt { report } => {
                Self::HistoricalCommitReceipt(report)
            }
            RecoveryOutcomeWireKind::RolledBack { receipt }
            | RecoveryOutcomeWireKind::HistoricalRollbackReceipt { receipt } => {
                Self::HistoricalRollbackReceipt(receipt)
            }
            RecoveryOutcomeWireKind::NoTransaction { recovery } => Self::NoTransaction(recovery),
        })
    }
}

impl RecoveryOutcome {
    #[must_use]
    pub const fn version(&self) -> u8 {
        RECOVERY_OUTCOME_VERSION
    }

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
    #[error(
        "recovery workspace installation {actual:?} does not match the journal installation required by its current revision; base {base:?}, committed {committed:?}"
    )]
    InstallationUnavailable {
        base: WorkspaceInstallationDigest,
        committed: WorkspaceInstallationDigest,
        actual: WorkspaceInstallationDigest,
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

#[derive(Debug)]
struct ObservedProtocol {
    state: PublicationState,
    blocked_reason: Option<String>,
}

#[derive(Debug)]
struct RecoveryObservation {
    events: ObservedProtocol,
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
    security_metadata: Option<SecurityMetadataCopyReservation>,
    target_parent_identity: DirectoryIdentity,
    old_digest: Option<DigestV1>,
    old_identity: Option<FileIdentity>,
    new_digest: DigestV1,
    new_identity: FileIdentity,
}

struct RecoveryProgram {
    steps: Vec<RecoveryStep>,
    event_keys: Vec<PublicationAction>,
}

fn recovery_program(
    observation: &RecoveryObservation,
    direction: RecoveryDirection,
    finalize_workspace: bool,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryProgram, ObservationError> {
    let capacity = observation
        .artifacts
        .len()
        .checked_mul(4)
        .and_then(|events| events.checked_add(6))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "recovery execution steps",
        })?;
    let mut steps = recovery_vec(capacity, "recovery execution steps", budget)?;
    append_recovery_program(
        &observation.events.state,
        &observation.artifacts,
        direction,
        finalize_workspace,
        &mut steps,
    )
    .map_err(map_protocol_plan_error)?;
    let event_count = steps.iter().filter(|step| step.records_event()).count();
    let mut event_keys = recovery_vec(event_count, "recovery event plan keys", budget)?;
    event_keys.extend(
        steps
            .iter()
            .copied()
            .filter(|step| step.records_event())
            .map(RecoveryStep::action),
    );
    Ok(RecoveryProgram { steps, event_keys })
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
    let has_manifest = match open_journal_directory(&access, layout.directory()) {
        Ok(directory) => {
            match open_journal_regular_in_directory(&directory, layout.manifest_path()) {
                Ok(manifest) => {
                    drop(manifest);
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(_) => {
                    return Err(blocked(
                        locator,
                        invalid_journal("canonical manifest is not a regular file".to_owned()),
                    ));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(blocked(locator, io_reason(error))),
    };
    if has_manifest {
        let mut journal = Journal::open_in_access(layout, &access, budget)
            .map_err(|error| map_journal_open_error(locator, error))?;
        recover_open_journal(
            workspace.as_deref_mut(),
            &mut journal,
            locator,
            intent,
            budget,
        )
    } else {
        recover_prepared_transaction(workspace.as_deref(), &layout, locator, &access, budget)
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
    if let Some(workspace) = workspace {
        validate_preparation_workspace(preparation.document(), workspace, locator)?;
    }
    let receipt = RollbackReceipt::new(
        preparation.document().workspace_id(),
        preparation.document().base_revision(),
        preparation.document().base_installation(),
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
        validate_preparation_workspace(rollback.document(), workspace, locator)?;
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
        rollback.document().base_installation(),
        locator.clone(),
    )))
}

fn validate_preparation_workspace(
    preparation: &JournalPreparation,
    workspace: &AssetWorkspace,
    locator: &RecoveryLocator,
) -> Result<(), RecoveryError> {
    if preparation.workspace_id() != workspace.workspace_id() {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::WorkspaceMismatch {
                expected: preparation.workspace_id(),
                actual: workspace.workspace_id(),
            },
        ));
    }
    if preparation.base_revision() != workspace.revision() {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::BaselineUnavailable {
                expected: preparation.base_revision(),
                actual: workspace.revision(),
            },
        ));
    }
    if preparation.base_installation() != workspace.installation_digest() {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::InstallationUnavailable {
                base: preparation.base_installation(),
                committed: preparation.committed_installation(),
                actual: workspace.installation_digest(),
            },
        ));
    }
    Ok(())
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
        PremanifestCleanupError::Budget(source) => recovery_budget_error(locator, source),
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
    events: &ObservedProtocol,
    relation: WorkspaceBaselineRelation,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryOutcome, RecoveryError> {
    if events.state.abandoned() {
        // A rollback receipt is historical evidence. Its former target bytes
        // may have been superseded by a later publication, so terminal
        // redelivery must never inspect or restore them.
        return Ok(historical_rollback_receipt(&report));
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
        return historical_commit_outcome(report, locator, budget);
    };
    if relation == WorkspaceBaselineRelation::Diverged {
        return historical_commit_outcome(report, locator, budget);
    }
    let baseline = relation.protocol_observation();
    match baseline {
        BaselineObservation::Base | BaselineObservation::NotBase => {
            let may_be_partial = matches!(baseline, BaselineObservation::NotBase);
            // Installing a baseline changes in-memory state, so it remains a
            // stronger operation than receipt redelivery. Verify the current
            // publication image only in this branch before rebuilding it.
            let (execution, artifacts) = match observe_execution(journal, budget) {
                Ok(observation) => observation,
                Err(ObservationError::Blocked(_)) if may_be_partial => {
                    return historical_commit_outcome(report, locator, budget);
                }
                Err(error) => return Err(map_observation_error(locator, error)),
            };
            if artifacts.iter().any(|artifact| !artifact.is_published()) {
                if may_be_partial {
                    return historical_commit_outcome(report, locator, budget);
                }
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::UnexpectedEvidence {
                        artifact: "finalized-publication".to_owned(),
                    },
                ));
            }
            precharge_published_verification(journal, budget)
                .map_err(|error| map_observation_error(locator, error))?;
            let rebuilt =
                match prebuild_recovery_baseline(workspace, journal, &artifacts, locator, budget) {
                    Ok(rebuilt) => rebuilt,
                    Err(RecoveryError::Budget { locator, source }) => {
                        return Err(RecoveryError::Budget { locator, source });
                    }
                    Err(_) if may_be_partial => {
                        return historical_commit_outcome(report, locator, budget);
                    }
                    Err(error) => return Err(error),
                };
            let report = budgeted_commit_report(report, locator, budget)?;
            verify_and_install_recovery_baseline(
                journal,
                &artifacts,
                &execution,
                workspace,
                Some(&rebuilt),
                RecoveryBaselineExpectation::from_report(report.as_ref()),
            )
            .map_err(|error| map_execution_error(locator, error))?;
            Ok(commit_outcome(report, true))
        }
        BaselineObservation::Detached => {
            // The same workspace can legitimately have advanced through a
            // successor transaction. Redeliver the immutable receipt without
            // replacing its newer state.
            historical_commit_outcome(report, locator, budget)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceBaselineRelation {
    Base,
    Committed,
    Partial,
    Diverged,
    Detached,
}

#[derive(Debug, Clone, Copy)]
struct RecoveryBaselineExpectation {
    committed_revision: WorkspaceRevision,
    base_installation: WorkspaceInstallationDigest,
    committed_installation: WorkspaceInstallationDigest,
}

impl RecoveryBaselineExpectation {
    const fn from_report(report: &CommitReport) -> Self {
        Self {
            committed_revision: report.committed_revision(),
            base_installation: report.base_installation(),
            committed_installation: report.committed_installation(),
        }
    }
}

impl WorkspaceBaselineRelation {
    fn observe(report: &CommitReport, workspace: Option<&AssetWorkspace>) -> Self {
        let Some(workspace) = workspace else {
            return Self::Detached;
        };
        let revision = workspace.revision();
        let installation = workspace.installation_digest();
        if revision == report.base_revision() && installation == report.base_installation() {
            Self::Base
        } else if revision == report.committed_revision()
            && installation == report.committed_installation()
        {
            Self::Committed
        } else if revision == report.base_revision() || revision == report.committed_revision() {
            // A known logical state with a different physical installation must never be
            // reconstructed from this journal. This is the same-revision relocation case that
            // the installation digest exists to distinguish.
            Self::Diverged
        } else if installation == report.base_installation()
            || installation == report.committed_installation()
        {
            // Publication may leave a reopened workspace with a strict subset of the eventual
            // logical baseline while its complete physical topology still matches one journal
            // endpoint. Recovery may rebuild that partial logical view after filesystem proof.
            Self::Partial
        } else {
            Self::Detached
        }
    }

    const fn protocol_observation(self) -> BaselineObservation {
        match self {
            Self::Base => BaselineObservation::Base,
            Self::Committed | Self::Partial | Self::Diverged => BaselineObservation::NotBase,
            Self::Detached => BaselineObservation::Detached,
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

    let events = ObservedProtocol::from_journal(journal, budget)
        .map_err(|error| map_observation_error(locator, error))?;
    let relation = WorkspaceBaselineRelation::observe(&report, workspace.as_deref());
    let baseline = relation.protocol_observation();
    if events.state.finalized() {
        validate_manifest_paths(journal, budget)
            .map_err(|error| map_observation_error(locator, error))?;
        return recover_finalized_journal(
            workspace.as_deref_mut(),
            journal,
            locator,
            intent,
            report,
            &events,
            relation,
            budget,
        );
    }
    if relation == WorkspaceBaselineRelation::Diverged {
        let workspace = workspace
            .as_deref()
            .expect("diverged recovery relation requires an attached workspace");
        return Err(blocked(
            locator,
            RecoveryBlockedReason::InstallationUnavailable {
                base: report.base_installation(),
                committed: report.committed_installation(),
                actual: workspace.installation_digest(),
            },
        ));
    }
    if relation == WorkspaceBaselineRelation::Committed && !events.state.published() {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::InvalidEventSequence {
                message:
                    "the committed workspace installation predates the journal publication boundary"
                        .to_owned(),
            },
        ));
    }
    let (mut execution, artifacts) = observe_execution(journal, budget)
        .map_err(|error| map_observation_error(locator, error))?;
    let mut observation = RecoveryObservation {
        events,
        artifacts,
        baseline,
    };
    if workspace.is_some()
        && (!observation.events.state.published()
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
    if relation == WorkspaceBaselineRelation::Detached
        && let Some(workspace) = workspace.as_deref()
    {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::BaselineUnavailable {
                expected: report.committed_revision(),
                actual: workspace.revision(),
            },
        ));
    }
    let plan = decide_recovery(RecoveryRequest {
        intent,
        state: &observation.events.state,
        artifacts: &observation.artifacts,
        baseline: observation.baseline,
    });

    match plan {
        RecoveryDecision::Blocked(block) => {
            let reason = map_protocol_block(block, observation.events.blocked_reason.as_deref());
            if matches!(reason, RecoveryBlockedReason::InvalidEventSequence { .. })
                && observation.events.state.published()
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
            if observation.events.state.direction() == Some(RecoveryDirection::Forward)
                && !observation.events.state.published()
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
            block_and_record(journal, &mut observation.events, locator, reason, budget)
        }
        RecoveryDecision::Forward => {
            let finalize_workspace = workspace.is_some();
            let prebuilt_baseline = if matches!(
                observation.baseline,
                BaselineObservation::Base | BaselineObservation::NotBase
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
            let program = recovery_program(
                &observation,
                RecoveryDirection::Forward,
                finalize_workspace,
                budget,
            )
            .map_err(|error| map_observation_error(locator, error))?;
            let event_plan = journal
                .plan_events(&program.event_keys, budget)
                .map_err(|error| map_journal_mutation_error(locator, error))?;
            precharge_execution_verification(
                journal,
                &mut execution,
                &observation.artifacts,
                &program.steps,
                RecoveryDirection::Forward,
                budget,
            )
            .map_err(|error| map_observation_error(locator, error))?;
            let report = budgeted_commit_report(report, locator, budget)?;
            #[cfg(test)]
            super::test_run_publication_hook("before_recovery_execution");
            execute_forward_program(
                journal,
                &mut observation.events,
                &mut observation.artifacts,
                &mut execution,
                program.steps,
                event_plan,
                workspace.as_deref_mut(),
                prebuilt_baseline,
                RecoveryBaselineExpectation::from_report(report.as_ref()),
            )
            .map_err(|error| map_execution_error(locator, error))?;
            Ok(commit_outcome(report, workspace_attached))
        }
        RecoveryDecision::Rollback => {
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
            let program = recovery_program(&observation, RecoveryDirection::Rollback, true, budget)
                .map_err(|error| map_observation_error(locator, error))?;
            let event_plan = journal
                .plan_events(&program.event_keys, budget)
                .map_err(|error| map_journal_mutation_error(locator, error))?;
            precharge_execution_verification(
                journal,
                &mut execution,
                &observation.artifacts,
                &program.steps,
                RecoveryDirection::Rollback,
                budget,
            )
            .map_err(|error| map_observation_error(locator, error))?;
            #[cfg(test)]
            super::test_run_publication_hook("before_recovery_execution");
            execute_rollback_program(
                journal,
                &mut observation.events,
                &mut observation.artifacts,
                &execution,
                program.steps,
                event_plan,
            )
            .map_err(|error| map_execution_error(locator, error))?;
            Ok(rollback_outcome(&report))
        }
    }
}

fn rollback_outcome(report: &CommitReport) -> RecoveryOutcome {
    RecoveryOutcome::RolledBack(RollbackReceipt::new(
        report.workspace_id(),
        report.base_revision(),
        report.base_installation(),
        report.recovery().clone(),
    ))
}

fn budgeted_commit_report(
    report: CommitReport,
    locator: &RecoveryLocator,
    budget: &mut AssetLoadBudget,
) -> Result<Box<CommitReport>, RecoveryError> {
    let retained = u64::try_from(size_of::<CommitReport>()).map_err(|_| {
        recovery_budget_error(
            locator,
            BudgetError::ArithmeticOverflow {
                resource: "recovery commit report",
            },
        )
    })?;
    budget
        .check_bytes(retained)
        .map_err(|source| recovery_budget_error(locator, source))?;
    budget
        .consume_bytes(retained)
        .map_err(|source| recovery_budget_error(locator, source))?;
    Ok(Box::new(report))
}

fn commit_outcome(report: Box<CommitReport>, workspace_attached: bool) -> RecoveryOutcome {
    if workspace_attached {
        RecoveryOutcome::Finalized(report)
    } else {
        RecoveryOutcome::FilesystemRecovered(report)
    }
}

fn historical_commit_outcome(
    report: CommitReport,
    locator: &RecoveryLocator,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryOutcome, RecoveryError> {
    Ok(RecoveryOutcome::HistoricalCommitReceipt(
        budgeted_commit_report(report, locator, budget)?,
    ))
}

fn historical_rollback_receipt(report: &CommitReport) -> RecoveryOutcome {
    RecoveryOutcome::HistoricalRollbackReceipt(RollbackReceipt::new(
        report.workspace_id(),
        report.base_revision(),
        report.base_installation(),
        report.recovery().clone(),
    ))
}

impl ObservedProtocol {
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
        artifacts.extend(
            manifest
                .artifacts()
                .iter()
                .map(|artifact| ArtifactProgress::new(artifact.backup().is_some())),
        );
        let mut observed = Self {
            state: PublicationState::new(artifacts),
            blocked_reason: None,
        };

        for event in journal.events() {
            observed
                .replay_event(event, manifest.artifacts(), &by_target)
                .map_err(ObservationError::Blocked)?;
        }
        Ok(observed)
    }

    fn replay_event(
        &mut self,
        event: &JournalEvent,
        artifacts: &[JournalArtifact],
        by_target: &[usize],
    ) -> Result<(), RecoveryBlockedReason> {
        let protocol_event = match event.kind() {
            JournalEventKind::StagingVerified => {
                ProtocolEvent::Action(PublicationAction::StagingVerified)
            }
            JournalEventKind::Journaled => ProtocolEvent::Action(PublicationAction::Journaled),
            JournalEventKind::BackupIntent { artifact } => {
                ProtocolEvent::Action(PublicationAction::BackupIntent(event_artifact_ordinal(
                    artifacts, by_target, artifact,
                )?))
            }
            JournalEventKind::BackupCaptured { artifact } => {
                ProtocolEvent::Action(PublicationAction::BackupCaptured(event_artifact_ordinal(
                    artifacts, by_target, artifact,
                )?))
            }
            JournalEventKind::PromotionIntent { artifact } => {
                ProtocolEvent::Action(PublicationAction::PromotionIntent(event_artifact_ordinal(
                    artifacts, by_target, artifact,
                )?))
            }
            JournalEventKind::Promoted { artifact } => {
                ProtocolEvent::Action(PublicationAction::Promoted(event_artifact_ordinal(
                    artifacts, by_target, artifact,
                )?))
            }
            JournalEventKind::Published => ProtocolEvent::Action(PublicationAction::Published),
            JournalEventKind::BaselineInstalled => {
                ProtocolEvent::Action(PublicationAction::BaselineInstalled)
            }
            JournalEventKind::Finalized => ProtocolEvent::Action(PublicationAction::Finalized),
            JournalEventKind::RecoveryDecision { direction } => {
                ProtocolEvent::Action(PublicationAction::RecoveryDecision(*direction))
            }
            JournalEventKind::Abandoned => ProtocolEvent::Action(PublicationAction::Abandoned),
            JournalEventKind::RecoveryBlocked { reason } => {
                self.state
                    .apply(ProtocolEvent::RecoveryBlocked)
                    .map_err(map_protocol_error)?;
                self.blocked_reason = Some(reason.clone());
                return Ok(());
            }
            JournalEventKind::Marker { .. } => ProtocolEvent::LegacyMarker,
        };
        self.state.apply(protocol_event).map_err(map_protocol_error)
    }
}

fn event_artifact_ordinal(
    artifacts: &[JournalArtifact],
    by_target: &[usize],
    artifact: &super::journal::JournalPath,
) -> Result<u32, RecoveryBlockedReason> {
    let index = by_target
        .binary_search_by(|index| artifacts[*index].target().cmp(artifact))
        .map(|position| by_target[position])
        .map_err(|_| invalid_event("an event names an artifact outside the manifest"))?;
    u32::try_from(index).map_err(|_| invalid_event("artifact event ordinal overflowed"))
}

fn map_protocol_error(error: ProtocolError) -> RecoveryBlockedReason {
    match error {
        ProtocolError::ConflictingDecision => RecoveryBlockedReason::ConflictingDecision,
        error => invalid_event(error.to_string()),
    }
}

fn map_protocol_plan_error(error: ProtocolPlanError) -> RecoveryBlockedReason {
    match error {
        ProtocolPlanError::InvalidState(message) => invalid_event(message),
        ProtocolPlanError::UnexpectedEvidence { artifact } => {
            RecoveryBlockedReason::UnexpectedEvidence {
                artifact: format!("artifact-{artifact:08}"),
            }
        }
        ProtocolPlanError::ArtifactOrdinalOverflow => RecoveryBlockedReason::InvalidJournal {
            message: error.to_string(),
        },
    }
}

fn map_protocol_block(
    block: ProtocolBlock,
    previous_reason: Option<&str>,
) -> RecoveryBlockedReason {
    match block {
        ProtocolBlock::PreviousRecoveryBlocked => RecoveryBlockedReason::InvalidEventSequence {
            message: format!(
                "a previous recovery was blocked: {}",
                previous_reason.unwrap_or("reason unavailable")
            ),
        },
        ProtocolBlock::UnexpectedEvidence { artifact } => {
            RecoveryBlockedReason::UnexpectedEvidence {
                artifact: format!("artifact-{artifact:08}"),
            }
        }
        ProtocolBlock::InvalidEventSequence(message) => {
            RecoveryBlockedReason::InvalidEventSequence {
                message: message.to_owned(),
            }
        }
    }
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
        JournalError::Budget(source) => recovery_budget_error(locator, source),
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
                security_metadata: None,
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
    #[cfg(test)]
    super::test_record_verification_hash(length);
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

fn precharge_execution_verification(
    journal: &Journal,
    execution: &mut RecoveryExecutionPlan,
    observations: &[ArtifactObservation],
    steps: &[RecoveryStep],
    direction: RecoveryDirection,
    budget: &mut AssetLoadBudget,
) -> Result<(), ObservationError> {
    let plan = execution_verification_charge(journal, execution, observations, steps, direction)?;
    let security_bytes = u64::try_from(plan.security_metadata_copies)
        .map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "recovery security metadata reservations",
        })?
        .checked_mul(super::platform::SECURITY_METADATA_COPY_RESERVATION_BYTES)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "recovery security metadata reservations",
        })?;
    let total_bytes =
        plan.charge
            .bytes
            .checked_add(security_bytes)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "recovery execution verification bytes",
            })?;
    budget.check_entries(plan.charge.entries)?;
    budget.check_bytes(total_bytes)?;
    for step in steps {
        if let PublicationAction::BackupCaptured(ordinal) = step.action() {
            let index = verification_artifact_index(execution, observations, ordinal)?;
            execution.artifacts[index].security_metadata =
                Some(reserve_security_metadata_copy(budget)?);
        }
    }
    budget.consume_entries(plan.charge.entries)?;
    budget.consume_bytes(plan.charge.bytes)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutionVerificationPlan {
    charge: VerificationCharge,
    security_metadata_copies: usize,
}

fn execution_verification_charge(
    journal: &Journal,
    execution: &RecoveryExecutionPlan,
    observations: &[ArtifactObservation],
    steps: &[RecoveryStep],
    direction: RecoveryDirection,
) -> Result<ExecutionVerificationPlan, ObservationError> {
    if observations.len() != journal.manifest().artifacts().len()
        || execution.artifacts.len() != observations.len()
    {
        return Err(RecoveryBlockedReason::InvalidJournal {
            message: "recovery execution observations are incomplete".to_owned(),
        }
        .into());
    }
    let mut charge = VerificationCharge::default();
    let mut security_metadata_copies = 0_usize;
    for step in steps {
        match step.action() {
            PublicationAction::BackupIntent(ordinal) => {
                let index = verification_artifact_index(execution, observations, ordinal)?;
                let artifact = &journal.manifest().artifacts()[index];
                add_old_verification_reads(&mut charge, artifact, 1)?;
            }
            PublicationAction::BackupCaptured(ordinal) => {
                let index = verification_artifact_index(execution, observations, ordinal)?;
                let artifact = &journal.manifest().artifacts()[index];
                let old_reads = backup_capture_old_reads(observations[index])
                    .ok_or_else(|| invalid_event("backup verification evidence changed"))?;
                add_old_verification_reads(&mut charge, artifact, old_reads)?;
                security_metadata_copies = security_metadata_copies.checked_add(1).ok_or(
                    BudgetError::ArithmeticOverflow {
                        resource: "recovery security metadata reservations",
                    },
                )?;
            }
            PublicationAction::PromotionIntent(ordinal) => {
                let index = verification_artifact_index(execution, observations, ordinal)?;
                add_verification_reads(
                    &mut charge,
                    journal.manifest().artifacts()[index].new_identity(),
                    1,
                )?;
            }
            PublicationAction::Promoted(ordinal) => {
                let index = verification_artifact_index(execution, observations, ordinal)?;
                let new_reads = promoted_new_reads(observations[index])
                    .ok_or_else(|| invalid_event("promotion verification evidence changed"))?;
                add_verification_reads(
                    &mut charge,
                    journal.manifest().artifacts()[index].new_identity(),
                    new_reads,
                )?;
            }
            PublicationAction::Published | PublicationAction::BaselineInstalled => {
                add_published_verification_reads(journal, &mut charge)?;
            }
            PublicationAction::Finalized if direction == RecoveryDirection::Forward => {
                add_published_verification_reads(journal, &mut charge)?;
            }
            PublicationAction::Abandoned => {
                for (artifact, observation) in
                    journal.manifest().artifacts().iter().zip(observations)
                {
                    add_rollback_verification_reads(&mut charge, artifact, *observation)?;
                }
            }
            PublicationAction::StagingVerified
            | PublicationAction::Journaled
            | PublicationAction::Finalized
            | PublicationAction::RecoveryDecision(_) => {}
        }
    }
    Ok(ExecutionVerificationPlan {
        charge,
        security_metadata_copies,
    })
}

fn add_published_verification_reads(
    journal: &Journal,
    charge: &mut VerificationCharge,
) -> Result<(), ObservationError> {
    for artifact in journal.manifest().artifacts() {
        add_verification_reads(charge, artifact.new_identity(), 1)?;
        if artifact.old_identity().is_some() {
            add_old_verification_reads(charge, artifact, 1)?;
        }
        add_verification_entries(charge, 1)?;
    }
    Ok(())
}

fn precharge_published_verification(
    journal: &Journal,
    budget: &mut AssetLoadBudget,
) -> Result<(), ObservationError> {
    let mut charge = VerificationCharge::default();
    add_published_verification_reads(journal, &mut charge)?;
    budget.check_entries(charge.entries)?;
    budget.check_bytes(charge.bytes)?;
    budget.consume_entries(charge.entries)?;
    budget.consume_bytes(charge.bytes)?;
    Ok(())
}

fn backup_capture_old_reads(observation: ArtifactObservation) -> Option<u64> {
    match observation {
        ArtifactObservation {
            target: EntryEvidence::Old,
            staging: EntryEvidence::New,
            backup: EntryEvidence::Missing,
            had_original: true,
        } => Some(3),
        ArtifactObservation {
            target: EntryEvidence::Missing,
            staging: EntryEvidence::New,
            backup: EntryEvidence::Old,
            had_original: true,
        } => Some(1),
        _ => None,
    }
}

fn promoted_new_reads(observation: ArtifactObservation) -> Option<u64> {
    match (observation.target, observation.staging) {
        (EntryEvidence::New, EntryEvidence::Missing) => Some(1),
        (EntryEvidence::Old | EntryEvidence::Missing, EntryEvidence::New) => Some(3),
        _ => None,
    }
}

fn verification_artifact_index(
    execution: &RecoveryExecutionPlan,
    observations: &[ArtifactObservation],
    ordinal: u32,
) -> Result<usize, ObservationError> {
    let index = usize::try_from(ordinal)
        .map_err(|_| invalid_event("recovery verification ordinal overflowed"))?;
    observations
        .get(index)
        .ok_or_else(|| invalid_event("recovery verification observation is missing"))?;
    let artifact = execution
        .artifacts
        .get(index)
        .ok_or_else(|| invalid_event("recovery verification execution plan is missing"))?;
    if artifact.ordinal != ordinal {
        return Err(
            invalid_event("recovery verification artifact ordinals are not contiguous").into(),
        );
    }
    Ok(index)
}

fn add_old_verification_reads(
    charge: &mut VerificationCharge,
    artifact: &JournalArtifact,
    count: u64,
) -> Result<(), ObservationError> {
    let identity =
        artifact
            .old_identity()
            .ok_or_else(|| RecoveryBlockedReason::InvalidJournal {
                message: "existing artifact has no old identity".to_owned(),
            })?;
    add_verification_reads(charge, identity, count)?;
    Ok(())
}

fn add_rollback_verification_reads(
    charge: &mut VerificationCharge,
    artifact: &JournalArtifact,
    observation: ArtifactObservation,
) -> Result<(), ObservationError> {
    let cost = rollback_verification_cost(observation)
        .ok_or_else(|| invalid_event("rollback verification evidence changed"))?;
    if cost.old_reads != 0 {
        add_old_verification_reads(charge, artifact, cost.old_reads)?;
    }
    add_verification_reads(charge, artifact.new_identity(), cost.new_reads)?;
    add_verification_entries(charge, cost.entry_checks)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VerificationCost {
    old_reads: u64,
    new_reads: u64,
    entry_checks: u64,
}

fn rollback_verification_cost(observation: ArtifactObservation) -> Option<VerificationCost> {
    let (old_reads, new_reads, entry_checks) = if observation.had_original {
        match (observation.target, observation.staging, observation.backup) {
            (
                EntryEvidence::Old,
                EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                EntryEvidence::Missing,
            ) => (1, 0, 2),
            (
                EntryEvidence::Missing,
                EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                EntryEvidence::Old,
            ) => (4, 0, 2),
            (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::Old) => (4, 2, 2),
            (EntryEvidence::CorruptNew, EntryEvidence::Missing, EntryEvidence::Old) => (4, 0, 2),
            (
                EntryEvidence::Missing,
                EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                EntryEvidence::CorruptOld,
            )
            | (EntryEvidence::CorruptNew, EntryEvidence::Missing, EntryEvidence::CorruptOld) => {
                (0, 0, 0)
            }
            (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::CorruptOld) => (0, 2, 0),
            _ => return None,
        }
    } else {
        match (observation.target, observation.staging, observation.backup) {
            (
                EntryEvidence::Missing,
                EntryEvidence::Missing | EntryEvidence::New | EntryEvidence::CorruptNew,
                EntryEvidence::Missing,
            ) => (0, 0, 2),
            (EntryEvidence::New, EntryEvidence::Missing, EntryEvidence::Missing) => (0, 2, 2),
            (EntryEvidence::CorruptNew, EntryEvidence::Missing, EntryEvidence::Missing) => {
                (0, 0, 2)
            }
            _ => return None,
        }
    };
    Some(VerificationCost {
        old_reads,
        new_reads,
        entry_checks,
    })
}

fn add_verification_entries(
    charge: &mut VerificationCharge,
    count: u64,
) -> Result<(), BudgetError> {
    charge.entries = charge
        .entries
        .checked_add(count)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "recovery verification entries",
        })?;
    Ok(())
}

fn add_verification_reads(
    charge: &mut VerificationCharge,
    identity: &FileIdentity,
    count: u64,
) -> Result<(), BudgetError> {
    charge.entries = charge
        .entries
        .checked_add(count)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "recovery verification entries",
        })?;
    charge.bytes = charge
        .bytes
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
                    None,
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

fn execute_forward_program(
    journal: &mut Journal,
    protocol: &mut ObservedProtocol,
    observations: &mut [ArtifactObservation],
    execution: &mut RecoveryExecutionPlan,
    steps: Vec<RecoveryStep>,
    mut event_plan: JournalEventPlan,
    mut workspace: Option<&mut AssetWorkspace>,
    prebuilt_baseline: Option<super::baseline::PreparedBaseline>,
    expected: RecoveryBaselineExpectation,
) -> Result<(), ExecutionError> {
    if protocol.state.artifacts().len() != execution.artifacts.len()
        || observations.len() != execution.artifacts.len()
    {
        return Err(ExecutionError::Blocked(invalid_event(
            "recovery execution plan does not cover every artifact",
        )));
    }
    for step in steps {
        let action = step.action();
        let recorded = prepare_recovery_step(protocol, &mut event_plan, step)?;
        match action {
            PublicationAction::RecoveryDecision(RecoveryDirection::Forward)
            | PublicationAction::StagingVerified
            | PublicationAction::Journaled => {}
            PublicationAction::BackupIntent(ordinal) => {
                verify_recovery_backup_intent(observations, execution, ordinal)?;
            }
            PublicationAction::BackupCaptured(ordinal) => {
                execute_recovery_backup_capture(journal, observations, execution, ordinal)?;
                #[cfg(test)]
                if !step.records_event() {
                    super::test_run_publication_hook("after_recovery_backup_replay");
                }
            }
            PublicationAction::PromotionIntent(ordinal) => {
                verify_recovery_promotion_intent(journal, observations, execution, ordinal)?;
            }
            PublicationAction::Promoted(ordinal) => {
                execute_recovery_promotion(journal, observations, execution, ordinal)?;
                #[cfg(test)]
                if !step.records_event() {
                    super::test_run_publication_hook("after_recovery_promotion_replay");
                }
            }
            PublicationAction::Published => {
                verify_published_artifacts(journal, observations, execution)?;
            }
            PublicationAction::BaselineInstalled => {
                let workspace = workspace.as_deref_mut().ok_or_else(|| {
                    ExecutionError::Blocked(invalid_event(
                        "detached recovery cannot install a workspace baseline",
                    ))
                })?;
                verify_and_install_recovery_baseline(
                    journal,
                    observations,
                    execution,
                    workspace,
                    prebuilt_baseline.as_ref(),
                    expected,
                )?;
            }
            PublicationAction::Finalized => {
                if let Some(workspace) = workspace.as_deref_mut() {
                    verify_and_install_recovery_baseline(
                        journal,
                        observations,
                        execution,
                        workspace,
                        prebuilt_baseline.as_ref(),
                        expected,
                    )?;
                }
            }
            PublicationAction::RecoveryDecision(RecoveryDirection::Rollback)
            | PublicationAction::Abandoned => {
                return Err(ExecutionError::Blocked(invalid_event(
                    "forward recovery program contains a rollback action",
                )));
            }
        }
        if let Some((planned, transition)) = recorded {
            append_prepared_recovery_event(journal, protocol, planned, transition)?;
        }
    }
    ensure_recovery_event_plan_consumed(&mut event_plan)
}

fn execute_rollback_program(
    journal: &mut Journal,
    protocol: &mut ObservedProtocol,
    observations: &mut [ArtifactObservation],
    execution: &RecoveryExecutionPlan,
    steps: Vec<RecoveryStep>,
    mut event_plan: JournalEventPlan,
) -> Result<(), ExecutionError> {
    for step in steps {
        let action = step.action();
        let recorded = prepare_recovery_step(protocol, &mut event_plan, step)?;
        match action {
            PublicationAction::RecoveryDecision(RecoveryDirection::Rollback)
            | PublicationAction::Finalized => {}
            PublicationAction::Abandoned => {
                #[cfg(test)]
                super::test_run_publication_hook("before_recovery_rollback");
                roll_back(journal, observations, execution)?;
            }
            PublicationAction::RecoveryDecision(RecoveryDirection::Forward)
            | PublicationAction::StagingVerified
            | PublicationAction::Journaled
            | PublicationAction::BackupIntent(_)
            | PublicationAction::BackupCaptured(_)
            | PublicationAction::PromotionIntent(_)
            | PublicationAction::Promoted(_)
            | PublicationAction::Published
            | PublicationAction::BaselineInstalled => {
                return Err(ExecutionError::Blocked(invalid_event(
                    "rollback recovery program contains a forward action",
                )));
            }
        }
        let Some((planned, transition)) = recorded else {
            return Err(ExecutionError::Blocked(invalid_event(
                "rollback recovery program contains a physical replay",
            )));
        };
        append_prepared_recovery_event(journal, protocol, planned, transition)?;
    }
    ensure_recovery_event_plan_consumed(&mut event_plan)
}

fn prepare_recovery_step(
    protocol: &ObservedProtocol,
    event_plan: &mut JournalEventPlan,
    step: RecoveryStep,
) -> Result<Option<(PlannedJournalEvent, PreparedTransition)>, ExecutionError> {
    let action = step.action();
    if !step.records_event() {
        return if matches!(
            action,
            PublicationAction::BackupCaptured(_) | PublicationAction::Promoted(_)
        ) {
            Ok(None)
        } else {
            Err(ExecutionError::Blocked(invalid_event(
                "only completed filesystem actions may be replayed",
            )))
        };
    }
    let planned = event_plan.next().ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "recovery execution is missing a pre-encoded journal event",
        ))
    })?;
    if planned.action() != action {
        return Err(ExecutionError::Blocked(invalid_event(
            "recovery execution and journal event plans diverged",
        )));
    }
    let transition = protocol
        .state
        .prepare(action)
        .map_err(protocol_execution_error)?;
    Ok(Some((planned, transition)))
}

fn ensure_recovery_event_plan_consumed(
    event_plan: &mut JournalEventPlan,
) -> Result<(), ExecutionError> {
    if event_plan.next().is_some() {
        Err(ExecutionError::Blocked(invalid_event(
            "recovery journal plan contains an unexecuted event",
        )))
    } else {
        Ok(())
    }
}

fn verify_and_install_recovery_baseline(
    journal: &Journal,
    observations: &[ArtifactObservation],
    execution: &RecoveryExecutionPlan,
    workspace: &mut AssetWorkspace,
    prebuilt_baseline: Option<&super::baseline::PreparedBaseline>,
    expected: RecoveryBaselineExpectation,
) -> Result<(), ExecutionError> {
    #[cfg(test)]
    super::test_run_publication_hook("before_recovery_baseline_install");
    verify_published_artifacts(journal, observations, execution)?;
    let baseline = prebuilt_baseline.ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "recovery baseline was not prepared before execution",
        ))
    })?;
    match workspace.install_prepared_state(baseline.state()) {
        super::super::state::WorkspaceStateInstallOutcome::Installed
        | super::super::state::WorkspaceStateInstallOutcome::Unchanged => {
            if workspace.revision() != expected.committed_revision {
                return Err(ExecutionError::Blocked(
                    RecoveryBlockedReason::BaselineUnavailable {
                        expected: expected.committed_revision,
                        actual: workspace.revision(),
                    },
                ));
            }
            if workspace.installation_digest() != expected.committed_installation {
                return Err(ExecutionError::Blocked(
                    RecoveryBlockedReason::InstallationUnavailable {
                        base: expected.base_installation,
                        committed: expected.committed_installation,
                        actual: workspace.installation_digest(),
                    },
                ));
            }
            Ok(())
        }
        super::super::state::WorkspaceStateInstallOutcome::Stale => Err(ExecutionError::Blocked(
            RecoveryBlockedReason::BaselineUnavailable {
                expected: expected.committed_revision,
                actual: workspace.revision(),
            },
        )),
    }
}

fn recovery_artifact_index(
    observations: &[ArtifactObservation],
    execution: &RecoveryExecutionPlan,
    ordinal: u32,
) -> Result<usize, ExecutionError> {
    let index = usize::try_from(ordinal).map_err(|_| {
        ExecutionError::Blocked(invalid_event("recovery artifact ordinal overflowed"))
    })?;
    observations.get(index).ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "recovery action has no filesystem observation",
        ))
    })?;
    let artifact = execution.artifacts.get(index).ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "recovery action has no physical execution plan",
        ))
    })?;
    if artifact.ordinal != ordinal {
        return Err(ExecutionError::Blocked(invalid_event(
            "recovery execution artifact ordinals are not contiguous",
        )));
    }
    Ok(index)
}

fn verify_recovery_backup_intent(
    observations: &[ArtifactObservation],
    execution: &RecoveryExecutionPlan,
    ordinal: u32,
) -> Result<(), ExecutionError> {
    let index = recovery_artifact_index(observations, execution, ordinal)?;
    let observation = observations[index];
    let paths = &execution.artifacts[index];
    if !matches!(
        (observation.target, observation.staging, observation.backup),
        (
            EntryEvidence::Old,
            EntryEvidence::New,
            EntryEvidence::Missing
        )
    ) {
        return Err(unexpected_execution_artifact(ordinal));
    }
    let old = paths.old_digest.ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "backup intent names an artifact without an old digest",
        ))
    })?;
    let old_identity = paths.old_identity.as_ref().ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "backup intent names an artifact without an old identity",
        ))
    })?;
    verify_digest_precharged(
        &paths.target,
        old,
        old_identity,
        &paths.target_parent_identity,
    )
}

fn execute_recovery_backup_capture(
    journal: &Journal,
    observations: &mut [ArtifactObservation],
    execution: &mut RecoveryExecutionPlan,
    ordinal: u32,
) -> Result<(), ExecutionError> {
    let index = recovery_artifact_index(observations, execution, ordinal)?;
    let observation = observations[index];
    let paths = &mut execution.artifacts[index];
    let old = paths.old_digest.ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "backup completion names an artifact without an old digest",
        ))
    })?;
    let old_identity = paths.old_identity.as_ref().ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "backup completion names an artifact without an old identity",
        ))
    })?;
    let backup = paths.backup.as_ref().ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "backup completion names an artifact without a backup path",
        ))
    })?;
    match (observation.target, observation.staging, observation.backup) {
        (EntryEvidence::Old, EntryEvidence::New, EntryEvidence::Missing) => {
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
        }
        (EntryEvidence::Missing, EntryEvidence::New, EntryEvidence::Old) => {
            verify_journal_digest_precharged(
                journal.backup_directory(),
                backup,
                old,
                old_identity,
            )?;
        }
        _ => return Err(unexpected_execution_artifact(ordinal)),
    }
    copy_security_metadata_between_journal_directories(
        journal.backup_directory(),
        backup,
        journal.stage_directory(),
        &paths.staging,
        old_identity,
        &paths.new_identity,
        paths
            .security_metadata
            .as_mut()
            .ok_or_else(|| {
                ExecutionError::Blocked(invalid_event(
                    "recovery has no reserved security metadata budget",
                ))
            })?
            .budget_mut(),
    )
    .map_err(map_security_metadata_execution_error)
}

fn verify_recovery_promotion_intent(
    journal: &Journal,
    observations: &[ArtifactObservation],
    execution: &RecoveryExecutionPlan,
    ordinal: u32,
) -> Result<(), ExecutionError> {
    let index = recovery_artifact_index(observations, execution, ordinal)?;
    let observation = observations[index];
    let paths = &execution.artifacts[index];
    let expected_backup = if observation.had_original {
        EntryEvidence::Old
    } else {
        EntryEvidence::Missing
    };
    if (observation.target, observation.staging, observation.backup)
        != (EntryEvidence::Missing, EntryEvidence::New, expected_backup)
    {
        return Err(unexpected_execution_artifact(ordinal));
    }
    verify_journal_digest_precharged(
        journal.stage_directory(),
        &paths.staging,
        paths.new_digest,
        &paths.new_identity,
    )
}

fn execute_recovery_promotion(
    journal: &Journal,
    observations: &mut [ArtifactObservation],
    execution: &RecoveryExecutionPlan,
    ordinal: u32,
) -> Result<(), ExecutionError> {
    let index = recovery_artifact_index(observations, execution, ordinal)?;
    let observation = observations[index];
    let paths = &execution.artifacts[index];
    let expected_backup = if observation.had_original {
        EntryEvidence::Old
    } else {
        EntryEvidence::Missing
    };
    match (observation.target, observation.staging, observation.backup) {
        (EntryEvidence::Missing, EntryEvidence::New, backup) if backup == expected_backup => {
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
        }
        (EntryEvidence::New, EntryEvidence::Missing, backup) if backup == expected_backup => {
            verify_digest_precharged(
                &paths.target,
                paths.new_digest,
                &paths.new_identity,
                &paths.target_parent_identity,
            )?;
        }
        _ => return Err(unexpected_execution_artifact(ordinal)),
    }
    Ok(())
}

fn verify_published_artifacts(
    journal: &Journal,
    observations: &[ArtifactObservation],
    execution: &RecoveryExecutionPlan,
) -> Result<(), ExecutionError> {
    if observations.len() != execution.artifacts.len() {
        return Err(ExecutionError::Blocked(invalid_event(
            "published verification does not cover every artifact",
        )));
    }
    for (observation, paths) in observations.iter().zip(&execution.artifacts) {
        if !observation.is_published() {
            return Err(unexpected_execution_artifact(paths.ordinal));
        }
        verify_digest_precharged(
            &paths.target,
            paths.new_digest,
            &paths.new_identity,
            &paths.target_parent_identity,
        )?;
        if let Some(old) = paths.old_digest {
            let old_identity = paths.old_identity.as_ref().ok_or_else(|| {
                ExecutionError::Blocked(invalid_event("published replacement has no old identity"))
            })?;
            let backup = paths.backup.as_ref().ok_or_else(|| {
                ExecutionError::Blocked(invalid_event("published replacement has no backup path"))
            })?;
            verify_journal_digest_precharged(
                journal.backup_directory(),
                backup,
                old,
                old_identity,
            )?;
        }
        verify_journal_absent_precharged(journal.stage_directory(), &paths.staging)?;
    }
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
    verify_rolled_back_artifacts(journal, observations, execution)
}

fn verify_rolled_back_artifacts(
    journal: &Journal,
    observations: &[ArtifactObservation],
    execution: &RecoveryExecutionPlan,
) -> Result<(), ExecutionError> {
    if observations.len() != execution.artifacts.len() {
        return Err(ExecutionError::Blocked(invalid_event(
            "rollback verification does not cover every artifact",
        )));
    }
    for (observation, paths) in observations.iter().zip(&execution.artifacts) {
        if !observation.is_rolled_back() {
            return Err(unexpected_execution_artifact(paths.ordinal));
        }
        if let Some(old) = paths.old_digest {
            let old_identity = paths.old_identity.as_ref().ok_or_else(|| {
                ExecutionError::Blocked(invalid_event("rolled-back artifact has no old identity"))
            })?;
            verify_digest_precharged(
                &paths.target,
                old,
                old_identity,
                &paths.target_parent_identity,
            )?;
        } else {
            verify_absent_precharged(&paths.target, &paths.target_parent_identity)?;
        }
        if let Some(backup) = &paths.backup {
            verify_journal_absent_precharged(journal.backup_directory(), backup)?;
        }
        verify_journal_owned_or_absent_precharged(
            journal.stage_directory(),
            &paths.staging,
            observation.staging,
            &paths.new_identity,
        )?;
    }
    Ok(())
}

fn unexpected_execution_artifact(ordinal: u32) -> ExecutionError {
    ExecutionError::Blocked(RecoveryBlockedReason::UnexpectedEvidence {
        artifact: format!("artifact-{ordinal:08}"),
    })
}

fn append_prepared_recovery_event(
    journal: &mut Journal,
    protocol: &mut ObservedProtocol,
    planned: PlannedJournalEvent,
    transition: PreparedTransition,
) -> Result<(), ExecutionError> {
    journal.append_planned(planned)?;
    protocol.state.apply_prepared(transition);
    Ok(())
}

fn protocol_journal_error(error: ProtocolError) -> super::journal::JournalError {
    super::journal::JournalError::InvalidEvent(error.to_string())
}

fn protocol_execution_error(error: ProtocolError) -> ExecutionError {
    protocol_journal_error(error).into()
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
    #[cfg(test)]
    super::test_record_verification_hash(expected_identity.length());
    let actual = DigestV1::hash_reader(&mut file, expected_identity.length())?;
    if actual == expected {
        Ok(())
    } else {
        Err(unexpected_verification())
    }
}

fn verify_absent_precharged(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> Result<(), ExecutionError> {
    #[cfg(test)]
    super::test_record_verification_entry();
    match open_readonly_regular_in_parent(path, expected_parent) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Err(unexpected_verification()),
        Err(error) => Err(ExecutionError::Io(error)),
        Ok(_) => Err(unexpected_verification()),
    }
}

fn verify_journal_absent_precharged(
    directory: &JournalDirectory,
    path: &Path,
) -> Result<(), ExecutionError> {
    #[cfg(test)]
    super::test_record_verification_entry();
    match open_journal_regular_in_directory(directory, path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Err(unexpected_verification()),
        Err(error) => Err(ExecutionError::Io(error)),
        Ok(_) => Err(unexpected_verification()),
    }
}

fn verify_journal_owned_or_absent_precharged(
    directory: &JournalDirectory,
    path: &Path,
    evidence: EntryEvidence,
    expected_identity: &FileIdentity,
) -> Result<(), ExecutionError> {
    match evidence {
        EntryEvidence::Missing => verify_journal_absent_precharged(directory, path),
        EntryEvidence::New | EntryEvidence::CorruptNew => {
            #[cfg(test)]
            super::test_record_verification_entry();
            let file =
                open_journal_regular_in_directory(directory, path).map_err(ExecutionError::Io)?;
            if opened_file_identity(&file)? == *expected_identity {
                Ok(())
            } else {
                Err(unexpected_verification())
            }
        }
        _ => Err(unexpected_verification()),
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
    #[cfg(test)]
    super::test_record_verification_hash(expected_identity.length());
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
    protocol: &mut ObservedProtocol,
    locator: &RecoveryLocator,
    reason: RecoveryBlockedReason,
    budget: &mut AssetLoadBudget,
) -> Result<T, RecoveryError> {
    if !protocol.state.recovery_blocked() {
        protocol
            .state
            .validate(ProtocolEvent::RecoveryBlocked)
            .map_err(|error| {
                blocked(
                    locator,
                    RecoveryBlockedReason::InvalidEventSequence {
                        message: error.to_string(),
                    },
                )
            })?;
        let record = reason.to_string();
        journal
            .append(JournalEventKind::RecoveryBlocked { reason: record }, budget)
            .map_err(|error| map_journal_mutation_error(locator, error))?;
        protocol
            .state
            .apply(ProtocolEvent::RecoveryBlocked)
            .map_err(|error| {
                blocked(
                    locator,
                    RecoveryBlockedReason::InvalidEventSequence {
                        message: error.to_string(),
                    },
                )
            })?;
    }
    Err(blocked(locator, reason))
}

fn blocked(locator: &RecoveryLocator, reason: RecoveryBlockedReason) -> RecoveryError {
    RecoveryError::Blocked {
        locator: Box::new(locator.clone()),
        reason: Box::new(reason),
    }
}

fn recovery_budget_error(locator: &RecoveryLocator, source: BudgetError) -> RecoveryError {
    RecoveryError::Budget {
        locator: Box::new(locator.clone()),
        source,
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
        JournalError::Budget(source) => recovery_budget_error(locator, source),
        JournalError::Io(error) => blocked(locator, io_reason(error)),
        error => blocked(locator, invalid_journal(error.to_string())),
    }
}

fn map_journal_mutation_error(locator: &RecoveryLocator, error: JournalError) -> RecoveryError {
    match error {
        JournalError::Budget(source) => recovery_budget_error(locator, source),
        JournalError::Io(error) => blocked(locator, io_reason(error)),
        error => blocked(locator, invalid_journal(error.to_string())),
    }
}

fn map_baseline_error(
    locator: &RecoveryLocator,
    error: super::baseline::BaselineBuildError,
) -> RecoveryError {
    match error.into_budget() {
        Ok(source) => recovery_budget_error(locator, source),
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
        ObservationError::Budget(source) => recovery_budget_error(locator, source),
        ObservationError::Blocked(reason) => blocked(locator, reason),
    }
}

fn map_execution_error(locator: &RecoveryLocator, error: ExecutionError) -> RecoveryError {
    match error {
        ExecutionError::Budget(source) => recovery_budget_error(locator, source),
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
        AssetWorkspace, CommitError, FieldGuard, GenericMutation, MutationPlan, MutationValue,
        PlanPayload, PrepareOptions, PublicationTarget, SourceAdmissionBatch,
        SourceAdmissionOperation, SourceAdmissionPolicy, SourceExpectation, SourceOpenRequest,
        WorkspaceLookup, WorkspaceOptions, WorkspaceView,
    };
    use crate::{
        AssetLoadBudget, FieldPath, ObjectAddress, SourceAlias, SourceFingerprint, SourceId,
        SourceKind, SourceLocator, UnityClass, UnityValue,
    };

    use super::super::RECOVERY_LOCATOR_VERSION;
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
            "1".parse().unwrap(),
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
        let locator = SourceLocator::path(SOURCE_ALIAS).expect("source locator");
        let source = workspace
            .snapshot()
            .resolve_source(&locator, &mut AssetLoadBudget::default())
            .expect("resolve source for mutation plan");
        let WorkspaceLookup::Resolved(source) = source else {
            panic!("mutation plan source must resolve");
        };
        MutationPlan::new(
            workspace.workspace_id(),
            workspace.revision(),
            vec![SourceExpectation::new(locator, source.fingerprint())],
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
            "1".parse().unwrap(),
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
            workspace.workspace_id(),
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
        let shared_reference_store = Arc::clone(workspace.snapshot().reference_store());
        let shared_cache_before = shared_reference_store.local_entry_counts();
        let prepared = workspace
            .prepare(
                mutation_plan(&workspace, "Before", "After"),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare mutation");
        assert_eq!(
            shared_reference_store.local_entry_counts(),
            shared_cache_before,
            "preparing a candidate must not publish into the committed reference cache"
        );
        let candidate_cache = prepared.view().local_reference_cache_counts();
        assert!(candidate_cache.0 >= 1);
        assert_eq!(candidate_cache.1, 1);
        let report = workspace
            .commit(
                prepared,
                PublicationTarget::in_place(directory.path()).expect("publication target"),
                &mut AssetLoadBudget::default(),
            )
            .expect("commit fixture");
        (directory, path, workspace, report)
    }

    fn replace_with_same_bytes(path: &Path) {
        let bytes = fs::read(path).expect("replacement source bytes");
        let replacement = path.with_extension("external-replacement.tmp");
        fs::write(&replacement, bytes).expect("replacement file");
        fs::remove_file(path).expect("remove replaced target");
        fs::rename(replacement, path).expect("install replacement target");
    }

    fn relocate_streamed_source(
        workspace: &mut AssetWorkspace,
        source: SourceId,
        alias: &str,
        path: &Path,
        bytes: &[u8],
    ) {
        let mut budget = AssetLoadBudget::default();
        let mut relocation = SourceAdmissionBatch::with_capacity(2, &mut budget)
            .expect("reserve physical relocation batch");
        relocation
            .try_push(SourceAdmissionOperation::Unload(source), &mut budget)
            .expect("append previous physical binding removal");
        relocation
            .try_push(
                SourceAdmissionOperation::LoadBytes {
                    request: SourceOpenRequest::new(
                        path,
                        SourceAlias::new(alias).expect("streamed alias"),
                    )
                    .with_kind_hint(SourceKind::StreamedResource),
                    image: Arc::from(bytes),
                },
                &mut budget,
            )
            .expect("append replacement physical binding");
        let report = workspace
            .admit_sources(relocation, SourceAdmissionPolicy::Strict, &mut budget)
            .expect("relocate streamed source atomically");
        assert_eq!(report.outcomes()[0].disposition().source_id(), Some(source));
        assert_eq!(
            report.outcomes()[1].disposition().source_id(),
            Some(source),
            "relocation must preserve deterministic source identity"
        );
    }

    fn planned_recovery_verification_charge(
        report: &CommitReport,
        baseline: BaselineObservation,
        direction: RecoveryDirection,
        finalize_workspace: bool,
    ) -> VerificationCharge {
        let journal = Journal::open(
            test_layout_from_locator(report.recovery()).expect("journal layout"),
            &mut AssetLoadBudget::default(),
        )
        .expect("open journal");
        let events = ObservedProtocol::from_journal(&journal, &mut AssetLoadBudget::default())
            .expect("observe protocol");
        let (execution, artifacts) =
            observe_execution(&journal, &mut AssetLoadBudget::default()).expect("observe paths");
        let observation = RecoveryObservation {
            events,
            artifacts,
            baseline,
        };
        let program = recovery_program(
            &observation,
            direction,
            finalize_workspace,
            &mut AssetLoadBudget::default(),
        )
        .expect("recovery event program");
        execution_verification_charge(
            &journal,
            &execution,
            &observation.artifacts,
            &program.steps,
            direction,
        )
        .expect("recovery verification charge")
        .charge
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
    fn execution_verification_costs_cover_atomic_hash_passes() {
        let existing = |target, staging, backup| ArtifactObservation {
            target,
            staging,
            backup,
            had_original: true,
        };
        let absent = |target, staging| ArtifactObservation {
            target,
            staging,
            backup: EntryEvidence::Missing,
            had_original: false,
        };

        assert_eq!(
            backup_capture_old_reads(existing(
                EntryEvidence::Old,
                EntryEvidence::New,
                EntryEvidence::Missing,
            )),
            Some(3)
        );
        assert_eq!(
            backup_capture_old_reads(existing(
                EntryEvidence::Missing,
                EntryEvidence::New,
                EntryEvidence::Old,
            )),
            Some(1)
        );
        assert_eq!(
            promoted_new_reads(existing(
                EntryEvidence::Missing,
                EntryEvidence::New,
                EntryEvidence::Old,
            )),
            Some(3)
        );
        assert_eq!(
            promoted_new_reads(existing(
                EntryEvidence::New,
                EntryEvidence::Missing,
                EntryEvidence::Old,
            )),
            Some(1)
        );

        let cases = [
            (
                existing(
                    EntryEvidence::Old,
                    EntryEvidence::New,
                    EntryEvidence::Missing,
                ),
                VerificationCost {
                    old_reads: 1,
                    new_reads: 0,
                    entry_checks: 2,
                },
            ),
            (
                existing(
                    EntryEvidence::Missing,
                    EntryEvidence::New,
                    EntryEvidence::Old,
                ),
                VerificationCost {
                    old_reads: 4,
                    new_reads: 0,
                    entry_checks: 2,
                },
            ),
            (
                existing(
                    EntryEvidence::New,
                    EntryEvidence::Missing,
                    EntryEvidence::Old,
                ),
                VerificationCost {
                    old_reads: 4,
                    new_reads: 2,
                    entry_checks: 2,
                },
            ),
            (
                absent(EntryEvidence::New, EntryEvidence::Missing),
                VerificationCost {
                    old_reads: 0,
                    new_reads: 2,
                    entry_checks: 2,
                },
            ),
        ];
        for (observation, expected) in cases {
            assert_eq!(rollback_verification_cost(observation), Some(expected));
        }
    }

    #[test]
    fn terminal_verification_rejects_target_changes_after_observation() {
        let (_directory, published_path, _workspace, published_report, published) =
            published_restart_fixture();
        let published_layout =
            test_layout_from_locator(published_report.recovery()).expect("published layout");
        let published_journal = Journal::open(published_layout, &mut AssetLoadBudget::default())
            .expect("open published journal");
        let (published_execution, published_observations) =
            observe_execution(&published_journal, &mut AssetLoadBudget::default())
                .expect("observe published execution");
        replace_with_same_bytes(&published_path);
        assert_eq!(
            fs::read(&published_path).expect("replacement target"),
            published
        );
        assert!(matches!(
            verify_published_artifacts(
                &published_journal,
                &published_observations,
                &published_execution,
            ),
            Err(ExecutionError::Blocked(
                RecoveryBlockedReason::UnexpectedEvidence { .. }
            ))
        ));

        let (_directory, rollback_path, _workspace, rollback_report, _) =
            journaled_restart_fixture();
        let rollback_layout =
            test_layout_from_locator(rollback_report.recovery()).expect("rollback layout");
        let rollback_journal = Journal::open(rollback_layout, &mut AssetLoadBudget::default())
            .expect("open rollback journal");
        let (rollback_execution, rollback_observations) =
            observe_execution(&rollback_journal, &mut AssetLoadBudget::default())
                .expect("observe rollback execution");
        replace_with_same_bytes(&rollback_path);
        assert_eq!(fs::read(&rollback_path).expect("replacement target"), YAML);
        assert!(matches!(
            verify_rolled_back_artifacts(
                &rollback_journal,
                &rollback_observations,
                &rollback_execution,
            ),
            Err(ExecutionError::Blocked(
                RecoveryBlockedReason::UnexpectedEvidence { .. }
            ))
        ));
    }

    #[test]
    fn live_multi_artifact_publication_rejects_replacement_before_published() {
        let directory = tempfile::tempdir().expect("publication directory");
        let path = directory.path().join(RESOURCE_ALIAS);
        fs::write(&path, RESOURCE_YAML).expect("resource fixture");
        let mut workspace = open_crash_resource_workspace(&path, crash_workspace_id());
        let base_revision = workspace.revision();
        let prepared = workspace
            .prepare(
                resource_plan(&workspace),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare resource publication");
        let hook_path = path.clone();
        super::super::test_set_publication_hook("before_live_published_verification", move || {
            replace_with_same_bytes(&hook_path)
        });

        let error = workspace
            .commit(
                prepared,
                PublicationTarget::in_place(directory.path()).expect("publication target"),
                &mut AssetLoadBudget::default(),
            )
            .expect_err("replacement before Published must require recovery");
        let CommitError::RecoveryRequired { locator, .. } = error else {
            panic!("replacement must return RecoveryRequired");
        };

        assert_eq!(workspace.revision(), base_revision);
        let journal = Journal::open(
            test_layout_from_locator(&locator).expect("journal layout"),
            &mut AssetLoadBudget::default(),
        )
        .expect("open interrupted journal");
        assert!(
            !journal
                .events()
                .iter()
                .any(|event| matches!(event.kind(), JournalEventKind::Published))
        );
        assert_eq!(
            journal
                .events()
                .iter()
                .filter(|event| matches!(event.kind(), JournalEventKind::Promoted { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn live_verification_charge_matches_multi_artifact_execution() {
        let directory = tempfile::tempdir().expect("publication directory");
        let path = directory.path().join(RESOURCE_ALIAS);
        fs::write(&path, RESOURCE_YAML).expect("resource fixture");
        let mut workspace = open_crash_resource_workspace(&path, crash_workspace_id());
        let prepared = workspace
            .prepare(
                resource_plan(&workspace),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .expect("prepare resource publication");
        let target = PublicationTarget::in_place(directory.path()).expect("publication target");
        let preflight =
            super::super::preflight_commit(&prepared, &target, &mut AssetLoadBudget::default())
                .expect("preflight publication");
        let expected = super::super::live_execution_verification_charge(&preflight.publications)
            .expect("live verification charge");
        super::super::test_set_publication_hook("before_live_execution", || {
            super::super::test_begin_verification_measurement();
        });

        workspace
            .commit(prepared, target, &mut AssetLoadBudget::default())
            .expect("publish resource artifacts");

        assert_eq!(
            super::super::test_finish_verification_measurement(),
            expected
        );
    }

    #[test]
    fn recovery_verification_charges_match_forward_and_rollback_execution() {
        let (_forward_directory, _forward_path, _workspace, forward_report, _published) =
            journaled_restart_fixture();
        let forward_expected = planned_recovery_verification_charge(
            &forward_report,
            BaselineObservation::Detached,
            RecoveryDirection::Forward,
            false,
        );
        super::super::test_set_publication_hook("before_recovery_execution", || {
            super::super::test_begin_verification_measurement();
        });
        AssetWorkspace::recover_at(forward_report.recovery(), &mut AssetLoadBudget::default())
            .expect("forward recovery");
        assert_eq!(
            super::super::test_finish_verification_measurement(),
            forward_expected
        );

        let (_rollback_directory, _rollback_path, _workspace, rollback_report, _published) =
            journaled_restart_fixture();
        let rollback_expected = planned_recovery_verification_charge(
            &rollback_report,
            BaselineObservation::Detached,
            RecoveryDirection::Rollback,
            true,
        );
        super::super::test_set_publication_hook("before_recovery_execution", || {
            super::super::test_begin_verification_measurement();
        });
        AssetWorkspace::abandon_at(rollback_report.recovery(), &mut AssetLoadBudget::default())
            .expect("rollback recovery");
        assert_eq!(
            super::super::test_finish_verification_measurement(),
            rollback_expected
        );
    }

    #[test]
    fn published_baseline_charge_matches_recovery_execution() {
        let (_directory, _path, mut workspace, report, _published) = published_restart_fixture();
        let expected = planned_recovery_verification_charge(
            &report,
            BaselineObservation::Base,
            RecoveryDirection::Forward,
            true,
        );
        super::super::test_set_publication_hook("before_recovery_execution", || {
            super::super::test_begin_verification_measurement();
        });

        workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("published baseline recovery");

        assert_eq!(
            super::super::test_finish_verification_measurement(),
            expected
        );
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
    fn rollback_retry_one_short_budget_preserves_the_terminal_receipt() {
        let retry_fixture = || {
            let (directory, path, locator) = run_crashing_commit("preparation_installed");
            let rolled_back = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
                .expect("create terminal rollback receipt");
            let receipt = rolled_back
                .rolled_back()
                .expect("premanifest recovery must roll back")
                .clone();
            let layout = test_layout_from_locator(&locator).expect("retry layout");
            assert!(layout.rollback_path().is_file());
            assert!(!layout.preparation_path().exists());

            let workspace = open_crash_workspace(&path, receipt.workspace_id());
            let prepared = workspace
                .prepare(
                    mutation_plan(&workspace, "Before", "After"),
                    PrepareOptions::default(),
                    &mut AssetLoadBudget::default(),
                )
                .expect("prepare rollback retry");
            (directory, path, locator, layout, workspace, prepared)
        };

        let (
            measured_directory,
            _measured_path,
            _measured_locator,
            _measured_layout,
            mut measured_workspace,
            measured_prepared,
        ) = retry_fixture();
        let _ = super::super::test_take_live_precharge();
        measured_workspace
            .commit(
                measured_prepared,
                PublicationTarget::in_place(measured_directory.path())
                    .expect("measured retry target"),
                &mut AssetLoadBudget::default(),
            )
            .expect("measure rollback retry");
        let (usage_before, verification_charge) =
            super::super::test_take_live_precharge().expect("measured live precharge");
        assert!(verification_charge.bytes > 0);

        let (directory, path, locator, layout, mut workspace, prepared) = retry_fixture();
        let max_bytes = usage_before
            .bytes
            .checked_add(verification_charge.bytes)
            .expect("one-short byte limit")
            - 1;
        let mut one_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes,
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .expect("one-short retry budget");
        let _ = super::super::test_take_live_precharge();

        let error = workspace
            .commit(
                prepared,
                PublicationTarget::in_place(directory.path()).expect("one-short retry target"),
                &mut one_short,
            )
            .expect_err("one-short verification budget must fail before durable replacement");

        assert!(matches!(error, CommitError::Budget { .. }));
        assert_eq!(
            super::super::test_take_live_precharge(),
            Some((usage_before, verification_charge))
        );
        assert_eq!(one_short.usage(), usage_before);
        assert!(layout.rollback_path().is_file());
        assert!(!layout.preparation_path().exists());
        assert!(!locator.root().exists());
        assert_target_unchanged(&path, YAML);
        assert!(
            AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
                .expect("redeliver preserved rollback receipt")
                .rolled_back()
                .is_some()
        );
    }

    #[test]
    fn rollback_retry_crash_after_preparation_install_preserves_both_records() {
        let (directory, path, locator) = run_crashing_commit("preparation_installed");
        let receipt = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("create terminal rollback receipt");
        assert!(receipt.rolled_back().is_some());
        let layout = test_layout_from_locator(&locator).expect("retry layout");
        assert!(!layout.preparation_path().exists());
        assert!(layout.rollback_path().is_file());

        run_crash_child(
            directory.path(),
            "preparation_installed_before_rollback_ack",
            None,
        );

        assert!(layout.preparation_path().is_file());
        assert!(layout.rollback_path().is_file());
        assert!(!layout.directory().exists());
        assert_target_unchanged(&path, YAML);

        let recovered = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("reconcile retry preparation with terminal rollback");
        assert_eq!(recovered, receipt);
        assert!(!layout.preparation_path().exists());
        assert!(layout.rollback_path().is_file());
        assert_target_unchanged(&path, YAML);
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

    fn baseline_installed_restart_fixture() -> (
        TempDir,
        std::path::PathBuf,
        AssetWorkspace,
        CommitReport,
        Vec<u8>,
    ) {
        let (directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let published = fs::read(&path).expect("published target");
        truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::BaselineInstalled)
        });
        drop(workspace);

        fs::write(&path, YAML).expect("restore base target for reopen");
        let reopened = reopen_base_workspace(workspace_id, &path);
        assert_eq!(reopened.revision(), report.base_revision());
        fs::write(&path, &published).expect("restore published target");
        (directory, path, reopened, report, published)
    }

    fn finalized_base_restart_fixture() -> (
        TempDir,
        std::path::PathBuf,
        AssetWorkspace,
        CommitReport,
        Vec<u8>,
    ) {
        let (directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let published = fs::read(&path).expect("published target");
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

    fn recorded_renames_moved_back_fixture(
        retained: fn(&JournalEventKind) -> bool,
    ) -> (
        TempDir,
        PathBuf,
        AssetWorkspace,
        CommitReport,
        Vec<u8>,
        JournalLayout,
    ) {
        let (directory, path, workspace, report) = committed_fixture();
        let workspace_id = workspace.workspace_id();
        let published = fs::read(&path).expect("published target");
        let (layout, artifact) = truncate_events_after(&report, retained);
        drop(workspace);

        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(&path, &staging, artifact.new_identity())
            .expect("move the promoted inode back to staging");
        capture_existing(
            &backup,
            &path,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("move the captured old inode back to the target");
        let reopened = reopen_base_workspace(workspace_id, &path);
        (directory, path, reopened, report, published, layout)
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
    fn finalized_receipt_reinstalls_a_base_workspace_before_redelivery() {
        let (_directory, _path, mut reopened, report, _published) =
            finalized_base_restart_fixture();

        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("install finalized baseline");

        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(reopened.revision(), report.committed_revision());
        assert_eq!(read_name(&reopened), "After");
    }

    #[test]
    fn finalized_receipt_rejects_same_bytes_replacement_before_cas() {
        let (_directory, path, mut reopened, report, published) = finalized_base_restart_fixture();
        let events = report.recovery().root().join(EVENTS_DIRECTORY);
        let before = fs::read_dir(&events).expect("events").count();
        let hook_path = path.clone();
        super::super::test_set_publication_hook("before_recovery_baseline_install", move || {
            replace_with_same_bytes(&hook_path)
        });

        let error = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("replacement before finalized baseline CAS must block");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(reopened.revision(), report.base_revision());
        assert_eq!(fs::read(&path).expect("replacement target"), published);
        assert_eq!(fs::read_dir(events).expect("events").count(), before);
    }

    #[test]
    fn committed_baseline_rechecks_published_identity_before_finalized() {
        let (_directory, path, mut workspace, report) = committed_fixture();
        truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::BaselineInstalled)
        });
        let events = report.recovery().root().join(EVENTS_DIRECTORY);
        let before = fs::read_dir(&events).expect("events").count();
        let hook_path = path.clone();
        super::super::test_set_publication_hook("before_recovery_execution", move || {
            replace_with_same_bytes(&hook_path)
        });

        let error = workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("replacement before finalized must block committed workspace");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(workspace.revision(), report.committed_revision());
        assert_eq!(fs::read_dir(events).expect("events").count(), before + 1);
        let journal = Journal::open(
            test_layout_from_locator(report.recovery()).expect("journal layout"),
            &mut AssetLoadBudget::default(),
        )
        .expect("open blocked finalization journal");
        assert!(matches!(
            journal.events().last().map(JournalEvent::kind),
            Some(JournalEventKind::RecoveryDecision {
                direction: RecoveryDirection::Forward
            })
        ));
        assert!(
            !journal
                .events()
                .iter()
                .any(|event| matches!(event.kind(), JournalEventKind::Finalized))
        );
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
    fn durable_baseline_event_replays_process_local_install_before_finalized() {
        let (_directory, _path, mut reopened, report, _published) =
            baseline_installed_restart_fixture();
        let events = report.recovery().root().join(EVENTS_DIRECTORY);
        let before = fs::read_dir(&events).expect("events").count();

        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("replay process-local baseline install");

        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(reopened.revision(), report.committed_revision());
        assert_eq!(read_name(&reopened), "After");
        assert_eq!(fs::read_dir(events).expect("events").count(), before + 2);
    }

    #[test]
    fn baseline_replay_rejects_same_bytes_replacement_before_cas() {
        let (_directory, path, mut reopened, report, published) =
            baseline_installed_restart_fixture();
        let events = report.recovery().root().join(EVENTS_DIRECTORY);
        let before = fs::read_dir(&events).expect("events").count();
        let hook_path = path.clone();
        super::super::test_set_publication_hook("before_recovery_baseline_install", move || {
            replace_with_same_bytes(&hook_path)
        });

        let error = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("replacement before baseline CAS must block recovery");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(reopened.revision(), report.base_revision());
        assert_eq!(fs::read(&path).expect("replacement target"), published);
        assert_eq!(fs::read_dir(&events).expect("events").count(), before + 1);
        let journal = Journal::open(
            test_layout_from_locator(report.recovery()).expect("journal layout"),
            &mut AssetLoadBudget::default(),
        )
        .expect("open blocked journal");
        assert!(
            !journal
                .events()
                .iter()
                .any(|event| matches!(event.kind(), JournalEventKind::Finalized))
        );
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
        assert!(matches!(
            error,
            RecoveryError::Budget {
                source: BudgetError::Exceeded {
                    resource: "bytes",
                    limit,
                    requested,
                },
                ..
            } if limit == usage.bytes - 1 && requested == usage.bytes
        ));
        let report_box_bytes =
            u64::try_from(size_of::<CommitReport>()).expect("commit report allocation size");
        assert_eq!(one_short.usage().bytes, usage.bytes - report_box_bytes);
        assert_eq!(short_workspace.revision(), short_report.base_revision());
        assert_eq!(fs::read(short_path).expect("published target"), published);
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);

        let (
            _entry_short_directory,
            entry_short_path,
            mut entry_short_workspace,
            entry_short_report,
            published,
        ) = published_restart_fixture();
        let events = entry_short_report.recovery().root().join(EVENTS_DIRECTORY);
        let event_count = fs::read_dir(&events).expect("events").count();
        let mut one_entry_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_entries: usage.entries - 1,
            ..exact_limits
        })
        .expect("one-entry-short budget");

        let error = entry_short_workspace
            .finalize_recovery_at(entry_short_report.recovery(), &mut one_entry_short)
            .expect_err("one-entry-short recovery must fail");
        assert!(matches!(error, RecoveryError::Budget { .. }));
        assert_eq!(
            entry_short_workspace.revision(),
            entry_short_report.base_revision()
        );
        assert_eq!(
            fs::read(entry_short_path).expect("published target"),
            published
        );
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);
    }

    #[test]
    fn durable_baseline_replay_obeys_exact_and_one_short_entry_budgets() {
        let (_measured_directory, _measured_path, mut measured_workspace, measured_report, _) =
            baseline_installed_restart_fixture();
        let mut measured = AssetLoadBudget::default();
        measured_workspace
            .finalize_recovery_at(measured_report.recovery(), &mut measured)
            .expect("measure durable baseline replay");
        let usage = measured.usage();
        let exact_limits = unity_asset_core::AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..unity_asset_core::AssetLoadLimits::default()
        };

        let (_exact_directory, _exact_path, mut exact_workspace, exact_report, _) =
            baseline_installed_restart_fixture();
        let mut exact = AssetLoadBudget::new(exact_limits).expect("exact replay budget");
        exact_workspace
            .finalize_recovery_at(exact_report.recovery(), &mut exact)
            .expect("exact durable baseline replay");
        assert_eq!(exact.usage(), usage);

        let (_short_directory, path, mut short_workspace, report, published) =
            baseline_installed_restart_fixture();
        let events = report.recovery().root().join(EVENTS_DIRECTORY);
        let event_count = fs::read_dir(&events).expect("events").count();
        let mut one_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_entries: usage.entries - 1,
            ..exact_limits
        })
        .expect("one-entry-short replay budget");

        let error = short_workspace
            .finalize_recovery_at(report.recovery(), &mut one_short)
            .expect_err("one-entry-short replay must fail before execution");

        assert!(matches!(error, RecoveryError::Budget { .. }));
        assert_eq!(short_workspace.revision(), report.base_revision());
        assert_eq!(fs::read(path).expect("published target"), published);
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
    fn replay_recovery_precharges_exact_and_one_short_budgets_before_mutation() {
        let (
            _measured_directory,
            _measured_path,
            _measured_workspace,
            measured_report,
            _published,
            _measured_layout,
        ) = recorded_renames_moved_back_fixture(|kind| {
            matches!(kind, JournalEventKind::Promoted { .. })
        });
        let mut measured = AssetLoadBudget::default();
        AssetWorkspace::recover_at(measured_report.recovery(), &mut measured)
            .expect("measure physical replay recovery");
        let usage = measured.usage();
        let exact_limits = unity_asset_core::AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..unity_asset_core::AssetLoadLimits::default()
        };

        let (_directory, path, mut workspace, report, published, layout) =
            recorded_renames_moved_back_fixture(|kind| {
                matches!(kind, JournalEventKind::Promoted { .. })
            });
        let journal = Journal::open(layout.clone(), &mut AssetLoadBudget::default())
            .expect("open replay journal before one-short probe");
        let artifact = journal.manifest().artifacts()[0].clone();
        drop(journal);
        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        let target_before = fs::read(&path).expect("target before replay");
        let staging_before = fs::read(&staging).expect("staging before replay");
        let event_count = fs::read_dir(layout.events_directory())
            .expect("events before replay")
            .count();
        assert!(!backup.exists());
        let mut one_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .expect("one-short replay budget");

        let error = AssetWorkspace::recover_at(report.recovery(), &mut one_short)
            .expect_err("one-short replay must fail before its sticky decision or rename");

        assert!(matches!(error, RecoveryError::Budget { .. }));
        assert_eq!(
            fs::read(&path).expect("unchanged replay target"),
            target_before
        );
        assert_eq!(
            fs::read(&staging).expect("unchanged replay staging"),
            staging_before
        );
        assert!(!backup.exists());
        assert_eq!(
            fs::read_dir(layout.events_directory())
                .expect("unchanged replay events")
                .count(),
            event_count
        );

        let mut exact = AssetLoadBudget::new(exact_limits).expect("exact replay budget");
        let recovered = AssetWorkspace::recover_at(report.recovery(), &mut exact)
            .expect("exact physical replay recovery");
        assert_eq!(exact.usage(), usage);
        assert_eq!(fs::read(&path).expect("replayed target"), published);
        assert!(recovered.requires_workspace_finalization());
        let finalized = workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("attach exact physical replay recovery");
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
    fn rollback_rejects_same_bytes_replacement_before_abandoned() {
        let (_directory, path, _reopened, report, _published) = journaled_restart_fixture();
        let hook_path = path.clone();
        super::super::test_set_publication_hook("before_recovery_rollback", move || {
            replace_with_same_bytes(&hook_path);
        });

        let error = AssetWorkspace::abandon_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("replacement before rollback must block recovery");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_eq!(fs::read(&path).expect("replacement target"), YAML);
        let journal = Journal::open(
            test_layout_from_locator(report.recovery()).expect("journal layout"),
            &mut AssetLoadBudget::default(),
        )
        .expect("open blocked rollback journal");
        assert!(!journal.events().iter().any(|event| matches!(
            event.kind(),
            JournalEventKind::Abandoned | JournalEventKind::Finalized
        )));
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
    fn recovery_replays_a_recorded_backup_capture_moved_back_to_the_target() {
        let (_directory, path, mut reopened, report, published, layout) =
            recorded_renames_moved_back_fixture(|kind| {
                matches!(kind, JournalEventKind::BackupCaptured { .. })
            });

        let recovered =
            AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("replay the recorded backup capture");
        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("attach replayed backup transaction");

        assert!(recovered.requires_workspace_finalization());
        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(fs::read(&path).expect("recovered target"), published);
        assert_eq!(reopened.revision(), report.committed_revision());
        let journal =
            Journal::open(layout, &mut AssetLoadBudget::default()).expect("open replayed journal");
        assert_eq!(
            journal
                .events()
                .iter()
                .filter(|event| matches!(event.kind(), JournalEventKind::BackupCaptured { .. }))
                .count(),
            1,
            "physical replay must not duplicate a durable completion event"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn recovery_restores_stage_security_metadata_after_recorded_backup_capture() {
        let (_directory, path, mut reopened, report, published, layout) =
            recorded_renames_moved_back_fixture(|kind| {
                matches!(kind, JournalEventKind::BackupCaptured { .. })
            });
        let journal = Journal::open(layout.clone(), &mut AssetLoadBudget::default())
            .expect("open recorded backup journal");
        let artifact = journal.manifest().artifacts()[0].clone();
        drop(journal);
        let staging = artifact.staging().join_root(layout.directory());
        let backup = artifact
            .backup()
            .expect("existing target backup")
            .join_root(layout.directory());
        capture_existing(
            &path,
            &backup,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("restore the captured backup topology");
        test_tamper_security_metadata(&staging).expect("tamper staged security metadata");
        assert!(
            !test_security_metadata_matches(&staging, &backup)
                .expect("compare tampered security metadata"),
            "the fixture must alter security metadata without changing the inode or bytes"
        );

        let recovered =
            AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("restore metadata and publish the staged inode");
        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("attach metadata-restored transaction");

        assert!(recovered.requires_workspace_finalization());
        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(fs::read(&path).expect("recovered target"), published);
        assert!(
            test_security_metadata_matches(&path, &backup)
                .expect("compare recovered security metadata"),
            "recovery must restore the captured target security metadata before promotion"
        );
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
    fn recovery_replays_recorded_renames_moved_back_to_the_previous_topology() {
        let (_directory, path, mut reopened, report, published, layout) =
            recorded_renames_moved_back_fixture(|kind| {
                matches!(kind, JournalEventKind::Promoted { .. })
            });

        let recovered =
            AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("replay both recorded renames");
        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("attach replayed promotion transaction");

        assert!(recovered.requires_workspace_finalization());
        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(fs::read(&path).expect("recovered target"), published);
        assert_eq!(reopened.revision(), report.committed_revision());
        let journal =
            Journal::open(layout, &mut AssetLoadBudget::default()).expect("open replayed journal");
        assert_eq!(
            journal
                .events()
                .iter()
                .filter(|event| matches!(event.kind(), JournalEventKind::BackupCaptured { .. }))
                .count(),
            1,
            "backup replay must not duplicate its durable completion event"
        );
        assert_eq!(
            journal
                .events()
                .iter()
                .filter(|event| matches!(event.kind(), JournalEventKind::Promoted { .. }))
                .count(),
            1,
            "promotion replay must not duplicate its durable completion event"
        );
    }

    #[test]
    fn replayed_renames_remain_retryable_after_each_physical_boundary() {
        for crash_point in [
            "after_recovery_backup_replay",
            "after_recovery_promotion_replay",
        ] {
            let (_directory, path, mut reopened, report, published, layout) =
                recorded_renames_moved_back_fixture(|kind| {
                    matches!(kind, JournalEventKind::Promoted { .. })
                });
            super::super::test_set_publication_hook(crash_point, || {
                panic!("injected replay interruption")
            });

            let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
            }));
            assert!(
                interrupted.is_err(),
                "{crash_point} must interrupt recovery"
            );
            let journal = Journal::open(layout.clone(), &mut AssetLoadBudget::default())
                .expect("open interrupted replay journal");
            assert_eq!(
                journal
                    .events()
                    .iter()
                    .filter(|event| matches!(
                        event.kind(),
                        JournalEventKind::RecoveryDecision {
                            direction: RecoveryDirection::Forward
                        }
                    ))
                    .count(),
                1,
                "the sticky decision must be durable exactly once"
            );
            assert_eq!(
                journal
                    .events()
                    .iter()
                    .filter(|event| matches!(event.kind(), JournalEventKind::BackupCaptured { .. }))
                    .count(),
                1,
                "backup replay must not append its completed event"
            );
            assert_eq!(
                journal
                    .events()
                    .iter()
                    .filter(|event| matches!(event.kind(), JournalEventKind::Promoted { .. }))
                    .count(),
                1,
                "promotion replay must not append its completed event"
            );
            drop(journal);

            let recovered =
                AssetWorkspace::recover_at(report.recovery(), &mut AssetLoadBudget::default())
                    .expect("retry interrupted physical replay");
            let outcome = reopened
                .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
                .expect("attach retried physical replay");

            assert!(recovered.requires_workspace_finalization());
            assert_eq!(
                outcome,
                RecoveryOutcome::Finalized(Box::new(report.clone()))
            );
            assert_eq!(fs::read(&path).expect("retried replay target"), published);
            assert_eq!(reopened.revision(), report.committed_revision());
            let journal = Journal::open(layout, &mut AssetLoadBudget::default())
                .expect("open finalized replay journal");
            assert_eq!(
                journal
                    .events()
                    .iter()
                    .filter(|event| matches!(event.kind(), JournalEventKind::BackupCaptured { .. }))
                    .count(),
                1
            );
            assert_eq!(
                journal
                    .events()
                    .iter()
                    .filter(|event| matches!(event.kind(), JournalEventKind::Promoted { .. }))
                    .count(),
                1
            );
        }
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
    fn finalized_receipt_completes_a_strict_partial_workspace_baseline() {
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
        drop(workspace);

        let mut reopened =
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default())
                .expect("reopened workspace");
        reopened
            .load_source(
                SourceOpenRequest::new(&path, SourceAlias::new(RESOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load published YAML without its companion");
        assert_ne!(reopened.revision(), report.base_revision());
        assert_ne!(reopened.revision(), report.committed_revision());

        let outcome = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("complete strict partial baseline");

        assert_eq!(
            outcome,
            RecoveryOutcome::Finalized(Box::new(report.clone()))
        );
        assert_eq!(reopened.revision(), report.committed_revision());
        assert_resource_payload(&reopened, &report);
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
    fn finalized_receipt_does_not_rebuild_over_same_revision_physical_relocation() {
        const STABLE_ALIAS: &str = "stable.resS";
        const STABLE_BYTES: &[u8] = b"stable physical binding";

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(SOURCE_ALIAS);
        let first_binding = directory.path().join("stable-first.resS");
        let second_binding = directory.path().join("stable-second.resS");
        fs::write(&path, YAML).expect("fixture bytes");
        fs::write(&first_binding, STABLE_BYTES).expect("first stable binding");
        fs::write(&second_binding, STABLE_BYTES).expect("second stable binding");

        let mut workspace = AssetWorkspace::new().expect("workspace");
        workspace
            .load_source(
                SourceOpenRequest::new(&path, SourceAlias::new(SOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load mutation source");
        let stable = workspace
            .load_source(
                SourceOpenRequest::new(
                    &first_binding,
                    SourceAlias::new(STABLE_ALIAS).expect("stable alias"),
                )
                .with_kind_hint(SourceKind::StreamedResource),
                &mut AssetLoadBudget::default(),
            )
            .expect("load stable source");

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
            .expect("commit mutation");
        assert_eq!(
            workspace.installation_digest(),
            report.committed_installation()
        );

        relocate_streamed_source(
            &mut workspace,
            stable,
            STABLE_ALIAS,
            &second_binding,
            STABLE_BYTES,
        );
        assert_eq!(workspace.revision(), report.committed_revision());
        assert_ne!(
            workspace.installation_digest(),
            report.committed_installation()
        );
        let relocated_installation = workspace.installation_digest();

        let outcome = workspace
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("redeliver finalized receipt after physical relocation");
        assert_eq!(
            outcome,
            RecoveryOutcome::HistoricalCommitReceipt(Box::new(report))
        );
        assert_eq!(workspace.installation_digest(), relocated_installation);
        assert_eq!(
            workspace
                .state()
                .catalog()
                .physical_origin(stable)
                .expect("relocated physical origin")
                .path(),
            fs::canonicalize(&second_binding)
                .expect("canonical second binding")
                .as_path()
        );
    }

    #[test]
    fn unfinished_recovery_blocks_same_revision_physical_relocation_before_mutation() {
        const STABLE_ALIAS: &str = "stable.resS";
        const STABLE_BYTES: &[u8] = b"stable physical binding";

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(SOURCE_ALIAS);
        let first_binding = directory.path().join("stable-first.resS");
        let second_binding = directory.path().join("stable-second.resS");
        fs::write(&path, YAML).expect("fixture bytes");
        fs::write(&first_binding, STABLE_BYTES).expect("first stable binding");
        fs::write(&second_binding, STABLE_BYTES).expect("second stable binding");

        let mut workspace = AssetWorkspace::new().expect("workspace");
        workspace
            .load_source(
                SourceOpenRequest::new(&path, SourceAlias::new(SOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load mutation source");
        workspace
            .load_source(
                SourceOpenRequest::new(
                    &first_binding,
                    SourceAlias::new(STABLE_ALIAS).expect("stable alias"),
                )
                .with_kind_hint(SourceKind::StreamedResource),
                &mut AssetLoadBudget::default(),
            )
            .expect("load stable source");
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
            .expect("commit mutation");
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

        let mut reopened =
            AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default())
                .expect("reopened workspace");
        reopened
            .load_source(
                SourceOpenRequest::new(&path, SourceAlias::new(SOURCE_ALIAS).expect("alias"))
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .expect("load base mutation source");
        let stable = reopened
            .load_source(
                SourceOpenRequest::new(
                    &first_binding,
                    SourceAlias::new(STABLE_ALIAS).expect("stable alias"),
                )
                .with_kind_hint(SourceKind::StreamedResource),
                &mut AssetLoadBudget::default(),
            )
            .expect("load base stable source");
        assert_eq!(reopened.revision(), report.base_revision());
        assert_eq!(reopened.installation_digest(), report.base_installation());
        relocate_streamed_source(
            &mut reopened,
            stable,
            STABLE_ALIAS,
            &second_binding,
            STABLE_BYTES,
        );
        assert_eq!(reopened.revision(), report.base_revision());
        assert_ne!(reopened.installation_digest(), report.base_installation());
        let target_before = fs::read(&path).expect("base target before blocked recovery");

        let error = reopened
            .finalize_recovery_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("unfinished recovery must reject unrelated physical relocation");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::InstallationUnavailable { .. })
        ));
        assert_target_unchanged(&path, &target_before);
        assert!(staging.is_file());
    }

    #[test]
    fn rollback_receipt_wire_is_versioned_strict_and_round_trippable() {
        let (_directory, _path, _workspace, report) = committed_fixture();
        let receipt = RollbackReceipt::new(
            report.workspace_id(),
            report.base_revision(),
            report.base_installation(),
            report.recovery().clone(),
        );
        let encoded = serde_json::to_value(&receipt).expect("serialize rollback receipt");
        assert_eq!(
            encoded,
            serde_json::json!({
                "version": ROLLBACK_RECEIPT_VERSION,
                "workspace_id": report.workspace_id(),
                "base_revision": report.base_revision(),
                "base_installation": report.base_installation(),
                "recovery": report.recovery(),
            })
        );
        assert_eq!(
            encoded["recovery"]["version"],
            serde_json::json!(RECOVERY_LOCATOR_VERSION)
        );
        let decoded = serde_json::from_value::<RollbackReceipt>(encoded.clone())
            .expect("deserialize rollback receipt");
        assert_eq!(decoded, receipt);
        assert_eq!(decoded.version(), ROLLBACK_RECEIPT_VERSION);

        let mut unsupported = encoded.clone();
        unsupported["version"] = serde_json::json!(ROLLBACK_RECEIPT_VERSION + 1);
        assert!(
            serde_json::from_value::<RollbackReceipt>(unsupported)
                .expect_err("unsupported rollback receipt version")
                .to_string()
                .contains("unsupported rollback receipt version")
        );

        let mut unknown = encoded;
        unknown
            .as_object_mut()
            .expect("rollback receipt object")
            .insert("unexpected".to_owned(), serde_json::Value::Null);
        assert!(
            serde_json::from_value::<RollbackReceipt>(unknown)
                .expect_err("unknown rollback receipt field")
                .to_string()
                .contains("unknown field")
        );
    }

    #[test]
    fn recovery_outcome_wire_is_stably_tagged_and_downgrades_live_claims() {
        let (_directory, _path, _workspace, report) = committed_fixture();
        let live = RecoveryOutcome::Finalized(Box::new(report.clone()));
        let encoded_text = serde_json::to_string(&live).expect("serialize live recovery outcome");
        assert!(
            encoded_text
                .starts_with("{\"version\":3,\"outcome\":{\"status\":\"finalized\",\"report\":")
        );
        let encoded =
            serde_json::from_str::<serde_json::Value>(&encoded_text).expect("outcome JSON");
        assert_eq!(
            encoded["version"],
            serde_json::json!(RECOVERY_OUTCOME_VERSION)
        );
        assert_eq!(encoded["outcome"]["status"], serde_json::json!("finalized"));
        let decoded = serde_json::from_value::<RecoveryOutcome>(encoded)
            .expect("deserialize live recovery outcome as history");
        assert_eq!(
            decoded,
            RecoveryOutcome::HistoricalCommitReceipt(Box::new(report.clone()))
        );
        assert!(decoded.finalized().is_none());
        assert!(decoded.historical_commit_receipt().is_some());
        assert_eq!(decoded.version(), RECOVERY_OUTCOME_VERSION);

        let historical = RecoveryOutcome::HistoricalCommitReceipt(Box::new(report));
        let round_trip = serde_json::from_value::<RecoveryOutcome>(
            serde_json::to_value(&historical).expect("serialize historical outcome"),
        )
        .expect("deserialize historical outcome");
        assert_eq!(round_trip, historical);
    }

    #[test]
    fn recovery_outcome_wire_downgrades_live_rollback_claims() {
        let (_directory, _path, _workspace, report) = committed_fixture();
        let receipt = RollbackReceipt::new(
            report.workspace_id(),
            report.base_revision(),
            report.base_installation(),
            report.recovery().clone(),
        );
        let live = RecoveryOutcome::RolledBack(receipt.clone());
        let encoded = serde_json::to_value(&live).expect("serialize live rollback outcome");
        assert_eq!(
            encoded["outcome"]["status"],
            serde_json::json!("rolled_back")
        );
        let decoded = serde_json::from_value::<RecoveryOutcome>(encoded)
            .expect("deserialize live rollback outcome as history");
        assert_eq!(decoded, RecoveryOutcome::HistoricalRollbackReceipt(receipt));
        assert!(decoded.rolled_back().is_none());
        assert!(decoded.historical_rollback_receipt().is_some());

        let absent = RecoveryOutcome::NoTransaction(report.recovery().clone());
        let round_trip = serde_json::from_value::<RecoveryOutcome>(
            serde_json::to_value(&absent).expect("serialize absent outcome"),
        )
        .expect("deserialize absent outcome");
        assert_eq!(round_trip, absent);
    }

    #[test]
    fn recovery_outcome_wire_rejects_unknown_versions_and_fields() {
        let (_directory, _path, _workspace, report) = committed_fixture();
        let outcome = RecoveryOutcome::HistoricalCommitReceipt(Box::new(report));
        let encoded = serde_json::to_value(&outcome).expect("serialize recovery outcome");

        let mut unsupported = encoded.clone();
        unsupported["version"] = serde_json::json!(RECOVERY_OUTCOME_VERSION + 1);
        assert!(
            serde_json::from_value::<RecoveryOutcome>(unsupported)
                .expect_err("unsupported recovery outcome version")
                .to_string()
                .contains("unsupported recovery outcome version")
        );

        let mut unknown_outer = encoded.clone();
        unknown_outer
            .as_object_mut()
            .expect("recovery outcome object")
            .insert("unexpected".to_owned(), serde_json::Value::Null);
        assert!(
            serde_json::from_value::<RecoveryOutcome>(unknown_outer)
                .expect_err("unknown recovery outcome field")
                .to_string()
                .contains("unknown field")
        );

        let mut unknown_variant = encoded;
        unknown_variant["outcome"]
            .as_object_mut()
            .expect("tagged recovery outcome")
            .insert("unexpected".to_owned(), serde_json::Value::Null);
        assert!(
            serde_json::from_value::<RecoveryOutcome>(unknown_variant)
                .expect_err("unknown tagged recovery outcome field")
                .to_string()
                .contains("unknown field")
        );
    }

    #[test]
    fn recovery_outcome_wire_validates_nested_receipts_and_reports() {
        let (_directory, _path, _workspace, report) = committed_fixture();
        let outcome = RecoveryOutcome::HistoricalCommitReceipt(Box::new(report.clone()));
        let encoded = serde_json::to_value(&outcome).expect("serialize recovery outcome");

        let mut unknown_report = encoded.clone();
        unknown_report["outcome"]["report"]
            .as_object_mut()
            .expect("nested commit report")
            .insert("unexpected".to_owned(), serde_json::Value::Null);
        assert!(
            serde_json::from_value::<RecoveryOutcome>(unknown_report)
                .expect_err("unknown nested commit report field")
                .to_string()
                .contains("unknown field")
        );

        let mut invalid_report = encoded;
        let base_revision = invalid_report["outcome"]["report"]["base_revision"].clone();
        invalid_report["outcome"]["report"]["committed_revision"] = base_revision;
        assert!(
            serde_json::from_value::<RecoveryOutcome>(invalid_report)
                .expect_err("invalid nested commit report")
                .to_string()
                .contains("revisions and change set disagree")
        );

        let receipt = RollbackReceipt::new(
            report.workspace_id(),
            report.base_revision(),
            report.base_installation(),
            report.recovery().clone(),
        );
        let rollback = RecoveryOutcome::HistoricalRollbackReceipt(receipt);
        let mut invalid_receipt =
            serde_json::to_value(&rollback).expect("serialize rollback outcome");
        invalid_receipt["outcome"]["receipt"]["version"] =
            serde_json::json!(ROLLBACK_RECEIPT_VERSION + 1);
        assert!(
            serde_json::from_value::<RecoveryOutcome>(invalid_receipt)
                .expect_err("invalid nested rollback receipt")
                .to_string()
                .contains("unsupported rollback receipt version")
        );

        let absent = RecoveryOutcome::NoTransaction(report.recovery().clone());
        let mut invalid_locator =
            serde_json::to_value(&absent).expect("serialize absent recovery outcome");
        invalid_locator["outcome"]["recovery"]["version"] =
            serde_json::json!(RECOVERY_LOCATOR_VERSION + 1);
        assert!(
            serde_json::from_value::<RecoveryOutcome>(invalid_locator)
                .expect_err("invalid nested recovery locator")
                .to_string()
                .contains("recovery locator version 2 is unsupported")
        );
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
