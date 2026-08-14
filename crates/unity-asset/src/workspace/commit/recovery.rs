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

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
#[cfg(all(test, unix))]
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
#[cfg(test)]
use unity_asset_core::DigestV1;
#[cfg(test)]
use unity_asset_core::TransactionId;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, WorkspaceId, WorkspaceRevision, vec_allocation_bytes,
};

use super::super::WorkspaceInstallationDigest;
#[cfg(test)]
use super::VerificationCharge;
#[cfg(test)]
use super::journal::{
    BACKUP_DIRECTORY, BASELINE_DIRECTORY, EVENTS_DIRECTORY, JournalArtifact, JournalEvent,
    JournalEventKind, STAGE_DIRECTORY,
};
use super::journal::{
    Journal, JournalError, JournalLayout, RECOVERY_DIRECTORY, RECOVERY_VERSION_DIRECTORY,
};
use super::platform::{
    CommitGuard, DirectoryEntryName, journal_access, observe_directory_identity, open_commit_root,
    open_existing_journal_namespace, open_journal_directory, open_journal_regular_in_directory,
};
#[cfg(test)]
use super::platform::{capture_existing, observe_file_identity};
#[cfg(all(test, any(unix, windows)))]
use super::platform::{test_security_metadata_matches, test_tamper_security_metadata};
use super::publication_protocol::RecoveryIntent;
#[cfg(test)]
use super::publication_protocol::{BaselineObservation, RecoveryDirection};
use super::{AssetWorkspace, CommitReport, RecoveryLocator};

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

fn ascii_directory_entry_name<'a>(
    entry: DirectoryEntryName<'_>,
    scratch: &'a mut [u8],
) -> Option<&'a str> {
    let length = entry.copy_ascii_into(scratch)?;
    std::str::from_utf8(&scratch[..length]).ok()
}

mod discovery;
pub(super) use discovery::discover_recoveries;

mod canonical;
use canonical::recover_open_journal;
#[cfg(test)]
use canonical::test_support::{
    VerificationError, observe_execution_for_test, planned_verification_charge,
    rollback_outcome_for_test,
};

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
            .map_err(|error| map_journal_error(locator, error))?;
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

mod premanifest;
use premanifest::recover_prepared_transaction;
pub(super) use premanifest::{
    PremanifestCleanupError, cleanup_orphaned_preparation_attempts, cleanup_prepared_transaction,
    cleanup_prepared_transaction_after_budget_exhaustion,
};

fn io_reason(error: io::Error) -> RecoveryBlockedReason {
    RecoveryBlockedReason::Io {
        message: error.to_string(),
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

fn map_journal_error(locator: &RecoveryLocator, error: JournalError) -> RecoveryError {
    match error {
        JournalError::Budget(source) => recovery_budget_error(locator, source),
        JournalError::Io(error) => blocked(locator, io_reason(error)),
        error => blocked(locator, invalid_journal(error.to_string())),
    }
}

fn map_observation_error(locator: &RecoveryLocator, error: ObservationError) -> RecoveryError {
    match error {
        ObservationError::Budget(source) => recovery_budget_error(locator, source),
        ObservationError::Blocked(reason) => blocked(locator, reason),
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
        let target = PublicationTarget::in_place(directory.path()).expect("publication target");
        let path = target.root().join(SOURCE_ALIAS);
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
            .commit(prepared, target, &mut AssetLoadBudget::default())
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
        planned_verification_charge(&journal, baseline, direction, finalize_workspace)
            .expect("recovery verification charge")
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
    fn terminal_verification_rejects_target_changes_after_observation() {
        let (_directory, published_path, _workspace, published_report, published) =
            published_restart_fixture();
        let published_layout =
            test_layout_from_locator(published_report.recovery()).expect("published layout");
        let published_journal = Journal::open(published_layout, &mut AssetLoadBudget::default())
            .expect("open published journal");
        let published_execution =
            observe_execution_for_test(&published_journal).expect("observe published execution");
        replace_with_same_bytes(&published_path);
        assert_eq!(
            fs::read(&published_path).expect("replacement target"),
            published
        );
        assert!(matches!(
            published_execution.verify_published(&published_journal),
            Err(VerificationError::Blocked(
                RecoveryBlockedReason::UnexpectedEvidence { .. }
            ))
        ));

        let (_directory, rollback_path, _workspace, rollback_report, _) =
            journaled_restart_fixture();
        let rollback_layout =
            test_layout_from_locator(rollback_report.recovery()).expect("rollback layout");
        let rollback_journal = Journal::open(rollback_layout, &mut AssetLoadBudget::default())
            .expect("open rollback journal");
        let rollback_execution =
            observe_execution_for_test(&rollback_journal).expect("observe rollback execution");
        replace_with_same_bytes(&rollback_path);
        assert_eq!(fs::read(&rollback_path).expect("replacement target"), YAML);
        assert!(matches!(
            rollback_execution.verify_rolled_back(&rollback_journal),
            Err(VerificationError::Blocked(
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
            let layout = test_layout_from_locator(&locator).expect("premanifest layout");
            assert_target_unchanged(&path, YAML);
            if point == "preparation_installed" {
                assert!(!locator.root().exists());
                assert!(layout.preparation_path().is_file());
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
                !layout.preparation_path().exists(),
                "{point} preparation record"
            );
            assert!(layout.rollback_path().is_file(), "{point} rollback receipt");
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

        assert_eq!(outcome, rollback_outcome_for_test(&report));
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

        assert_eq!(outcome, rollback_outcome_for_test(&report));
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
        let displaced = path.with_extension("externally-displaced");
        fs::rename(&path, &displaced).expect("retain original inode outside the target path");
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
        let target = PublicationTarget::in_place(directory.path()).expect("publication target");
        let path = target.root().join(SOURCE_ALIAS);
        let first_binding = target.root().join("stable-first.resS");
        let second_binding = target.root().join("stable-second.resS");
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
            .commit(prepared, target, &mut AssetLoadBudget::default())
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
        let target = PublicationTarget::in_place(directory.path()).expect("publication target");
        let path = target.root().join(SOURCE_ALIAS);
        let first_binding = target.root().join("stable-first.resS");
        let second_binding = target.root().join("stable-second.resS");
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
            .commit(prepared, target, &mut AssetLoadBudget::default())
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
        let root = open_commit_root(target.root(), target.identity()).expect("publication root");
        let _guard = CommitGuard::acquire_with_root(&root).expect("publication guard");

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
