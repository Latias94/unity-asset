//! Recovery and cleanup of transactions that have no canonical manifest.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError};

use super::super::journal::{
    BACKUP_DIRECTORY, BASELINE_DIRECTORY, EVENTS_DIRECTORY, JournalBaselineImage, JournalError,
    JournalLayout, JournalPreparation, MANIFEST_TEMPORARY_FILE, OpenedJournalPreparation,
    STAGE_DIRECTORY, matches_ordinal_journal_path,
};
use super::super::platform::{
    DIRECTORY_VISIT_ENTRY_BYTES, DIRECTORY_VISIT_SETUP_BYTES, DirectoryEntryName,
    DirectoryIdentity, DirectoryVisitError, FileIdentity, JournalAccess, JournalDirectory,
    capture_journal_regular, journal_directory_identity, open_journal_directory,
    open_journal_directory_in_directory, open_journal_regular, open_journal_regular_in_directory,
    opened_file_identity, remove_journal_directory, remove_journal_directory_in_directory,
    remove_journal_regular, remove_journal_regular_in_directory, sync_journal_access,
    visit_journal_directory_entries,
};
use super::super::{AssetWorkspace, RecoveryLocator};
use super::{
    MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES, ObservationError, RecoveryBlockedReason,
    RecoveryError, RecoveryOutcome, RollbackReceipt, ascii_directory_entry_name, blocked,
    io_reason, map_journal_open_error, map_observation_error, recovery_budget_error,
    recovery_join_component, recovery_vec,
};

pub(super) fn recover_prepared_transaction(
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
        PremanifestCleanupError::Io(error) => blocked(
            locator,
            RecoveryBlockedReason::Io {
                message: error.to_string(),
            },
        ),
    }
}

#[derive(Debug, Error)]
pub(in crate::workspace::commit) enum PremanifestCleanupError {
    #[error("premanifest cleanup exceeded its caller-owned budget: {0}")]
    Budget(#[source] BudgetError),
    #[error("premanifest cleanup was blocked by filesystem evidence: {0}")]
    Blocked(#[source] RecoveryBlockedReason),
    #[error("premanifest cleanup I/O failed: {0}")]
    Io(#[source] io::Error),
}

fn map_preparation_cleanup_error(error: JournalError) -> PremanifestCleanupError {
    match error {
        JournalError::Budget(error) => PremanifestCleanupError::Budget(error),
        JournalError::Io(error) => PremanifestCleanupError::Io(error),
        error => PremanifestCleanupError::Blocked(RecoveryBlockedReason::InvalidJournal {
            message: error.to_string(),
        }),
    }
}

pub(in crate::workspace::commit) fn cleanup_prepared_transaction(
    layout: &JournalLayout,
    access: &JournalAccess<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<(), PremanifestCleanupError> {
    let preparation = JournalPreparation::open_in_access(layout, access, budget)
        .map_err(map_preparation_cleanup_error)?;
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
pub(in crate::workspace::commit) fn cleanup_prepared_transaction_after_budget_exhaustion(
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

pub(in crate::workspace::commit) fn cleanup_orphaned_preparation_attempts(
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
            super::super::test_crash_failpoint("premanifest_rollback_captured");
            remove_journal_regular(
                access,
                layout.preparation_path(),
                plan.preparation.identity(),
            )?;
            sync_journal_access(access)?;
            #[cfg(test)]
            super::super::test_crash_failpoint("premanifest_rollback_recorded");
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

fn preparation_observation_error(error: JournalError) -> ObservationError {
    match error {
        JournalError::Budget(error) => ObservationError::Budget(error),
        error => ObservationError::Blocked(RecoveryBlockedReason::InvalidJournal {
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_cleanup_errors_preserve_their_operational_domain() {
        let budget = BudgetError::ArithmeticOverflow {
            resource: "premanifest cleanup test",
        };
        assert!(matches!(
            map_preparation_cleanup_error(JournalError::Budget(budget.clone())),
            PremanifestCleanupError::Budget(source) if source == budget
        ));

        assert!(matches!(
            map_preparation_cleanup_error(JournalError::Io(io::Error::new(
                io::ErrorKind::Interrupted,
                "premanifest cleanup test",
            ))),
            PremanifestCleanupError::Io(source)
                if source.kind() == io::ErrorKind::Interrupted
        ));

        assert!(matches!(
            map_preparation_cleanup_error(JournalError::UnsupportedVersion(u8::MAX)),
            PremanifestCleanupError::Blocked(RecoveryBlockedReason::InvalidJournal { .. })
        ));
    }
}
