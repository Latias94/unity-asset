//! Read-only discovery of canonical recovery evidence.

use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};

use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, TransactionId, vec_allocation_bytes,
};

use super::super::journal::{
    RECOVERY_DIRECTORY, RECOVERY_VERSION_DIRECTORY, RecoveryEvidenceName,
    parse_recovery_evidence_name,
};
use super::super::platform::{
    COMMIT_LOCK_FILE, CommitGuard, CommitLockPathError, CommitLockPaths,
    DIRECTORY_VISIT_ENTRY_BYTES, DIRECTORY_VISIT_SETUP_BYTES, DirectoryEntryName,
    DirectoryIdentity, DirectoryVisitError, LEGACY_COMMIT_LOCK_DIRECTORY,
    observe_directory_identity, open_readonly_regular_in_parent, visit_existing_directory_entries,
};
use super::super::{PublicationTarget, RecoveryLocator};
use super::{
    MAX_PROTOCOL_DIRECTORY_ENTRY_NAME_BYTES, RecoveryDiscovery, RecoveryDiscoveryBlockedReason,
    RecoveryDiscoveryError, ascii_directory_entry_name,
};

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

pub(in crate::workspace::commit) fn discover_recoveries(
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
