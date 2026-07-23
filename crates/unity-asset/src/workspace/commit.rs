//! Recoverable publication contracts for exact prepared artifacts.

use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ChangeSet, ContainmentKind, DigestV1, SourceFingerprint,
    SourceId, TransactionId, WorkspaceId, WorkspaceRevision,
};
use unity_asset_write::artifact::{ArtifactHandle, LogicalArtifactName};

use super::portable_path::{PortablePathError, slash_key};
use super::preflight::destination::{DestinationProofError, DestinationState};
use super::preflight::source_proof::PhysicalDependencyProofError;
use super::source_catalog::CatalogError;
use super::{AssetWorkspace, PreparedChange};

use self::baseline::{MaterializedImages, PreparedBaseline};
use self::journal::{
    Journal, JournalArtifact, JournalBaseline, JournalBaselineImage, JournalBaselineSource,
    JournalCatalogAction, JournalDirectoryIdentities, JournalError, JournalEventKey,
    JournalEventPlan, JournalExpectedDestination, JournalLayout, JournalManifest, JournalPath,
    JournalPreparation, JournalPreparationOutput, JournalTransactionOutputSeed,
    JournalTransactionSeed, OpenedJournalPreparation, transaction_id_from_seed,
};
use self::platform::{
    CommitGuard, CommitRoot, DirectoryIdentity, FileIdentity, JournalAccess, JournalDirectory,
    JournalNamespace, SecurityMetadataCopyReservation, SecurityMetadataError,
    capture_external_regular_in_journal_directory,
    copy_security_metadata_between_journal_directories,
    copy_security_metadata_external_to_journal_directory, create_journal_directory,
    create_journal_directory_in_directory, create_journal_regular_in_directory,
    ensure_journal_directory_same_filesystem, ensure_single_hardlink, journal_access,
    journal_directory_identity, observe_directory_identity, open_commit_root,
    open_journal_directory, open_journal_namespace, open_journal_regular,
    open_journal_regular_in_directory, open_readonly_regular_in_parent, opened_file_identity,
    promote_journal_regular_to_external, reserve_security_metadata_copy, sync_journal_access,
    sync_journal_directory, sync_journal_namespace,
};

mod baseline;
mod journal;
mod platform;
mod recovery;

#[cfg(test)]
const TEST_CRASH_POINT_ENV: &str = "UNITY_ASSET_TEST_CRASH_POINT";

#[cfg(test)]
fn test_crash_failpoint(point: &str) {
    if std::env::var(TEST_CRASH_POINT_ENV).is_ok_and(|configured| configured == point) {
        std::process::exit(86);
    }
}

#[cfg(test)]
fn test_crash_artifact_failpoint(point: &str, ordinal: u32) {
    if std::env::var(TEST_CRASH_POINT_ENV)
        .is_ok_and(|configured| configured == format!("{point}:{ordinal}"))
    {
        std::process::exit(86);
    }
}

pub use recovery::{
    RECOVERY_DISCOVERY_VERSION, RecoveryBlockedReason, RecoveryDiscovery,
    RecoveryDiscoveryBlockedReason, RecoveryDiscoveryError, RecoveryError, RecoveryOutcome,
    RollbackReceipt,
};

/// Current canonical commit-report wire version.
pub const COMMIT_REPORT_VERSION: u8 = 1;

/// Durability boundary promised for a publication set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitAtomicity {
    /// Each artifact replacement is atomic and the journal makes the set recoverable.
    PerArtifactRecoverable,
}

/// Stable public description of a destination observed during commit CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitDestinationState {
    Existing(SourceFingerprint),
    Absent,
    Directory,
    SymbolicLink,
    Other,
}

impl From<DestinationState> for CommitDestinationState {
    fn from(value: DestinationState) -> Self {
        match value {
            DestinationState::Existing(fingerprint) => Self::Existing(fingerprint),
            DestinationState::Absent => Self::Absent,
            DestinationState::Directory => Self::Directory,
            DestinationState::SymbolicLink => Self::SymbolicLink,
            DestinationState::Other => Self::Other,
        }
    }
}

/// Caller-selected containment and recovery boundary for already prepared destinations.
///
/// U7 publishes only the exact paths proven by [`PreparedChange`]. The root does not relocate
/// outputs; it constrains every target and owns the deterministic recovery namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationTarget {
    root: PathBuf,
    identity: DirectoryIdentity,
}

impl PublicationTarget {
    pub fn in_place(root: impl AsRef<Path>) -> Result<Self, PublicationTargetError> {
        let root = validate_publication_root(root.as_ref())?;
        let identity =
            observe_directory_identity(&root).map_err(|error| PublicationTargetError::Io {
                path: root.clone(),
                message: error.to_string(),
            })?;
        Ok(Self { root, identity })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn identity(&self) -> &DirectoryIdentity {
        &self.identity
    }

    pub(crate) fn verify_current(&self) -> io::Result<()> {
        if observe_directory_identity(&self.root)? != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "publication target root identity changed",
            ));
        }
        Ok(())
    }

    /// Rebuilds the deterministic journal locator for one transaction.
    ///
    /// Callers that persist only the transaction ID and the publication root
    /// can recover after a process restart without retaining a [`CommitReport`].
    #[must_use]
    pub fn recovery_locator(&self, transaction: TransactionId) -> RecoveryLocator {
        let layout = JournalLayout::new(&self.root, transaction, self.identity.clone());
        RecoveryLocator::new(
            layout.directory().to_path_buf(),
            transaction,
            layout.root_identity().clone(),
        )
    }

    /// Lists canonical v2 recovery candidates without opening a journal or
    /// modifying the publication namespace.
    pub fn discover_recoveries(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecoveryDiscovery, RecoveryDiscoveryError> {
        recovery::discover_recoveries(self, budget)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublicationTargetError {
    #[error("publication containment root must be absolute: {0:?}")]
    NotAbsolute(PathBuf),
    #[error("publication containment root must not be a symbolic link: {0:?}")]
    SymbolicLink(PathBuf),
    #[error("publication containment root is not a directory: {0:?}")]
    NotDirectory(PathBuf),
    #[error("failed to inspect publication containment root {path:?}: {message}")]
    Io { path: PathBuf, message: String },
}

fn validate_publication_root(root: &Path) -> Result<PathBuf, PublicationTargetError> {
    if !root.is_absolute() {
        return Err(PublicationTargetError::NotAbsolute(root.to_path_buf()));
    }
    let metadata = fs::symlink_metadata(root).map_err(|error| PublicationTargetError::Io {
        path: root.to_path_buf(),
        message: error.to_string(),
    })?;
    if metadata.file_type().is_symlink() {
        return Err(PublicationTargetError::SymbolicLink(root.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(PublicationTargetError::NotDirectory(root.to_path_buf()));
    }
    fs::canonicalize(root).map_err(|error| PublicationTargetError::Io {
        path: root.to_path_buf(),
        message: error.to_string(),
    })
}

/// Stable on-disk address for a recoverable or finalized transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryLocator {
    root: PathBuf,
    transaction: TransactionId,
    root_identity: DirectoryIdentity,
}

impl RecoveryLocator {
    pub(crate) fn new(
        root: PathBuf,
        transaction: TransactionId,
        root_identity: DirectoryIdentity,
    ) -> Self {
        Self {
            root,
            transaction,
            root_identity,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn transaction(&self) -> TransactionId {
        self.transaction
    }

    #[must_use]
    pub(crate) const fn root_identity(&self) -> &DirectoryIdentity {
        &self.root_identity
    }
}

/// One exact artifact included in a committed publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitArtifactReport {
    logical_name: String,
    source: SourceId,
    digest: DigestV1,
    bytes: u64,
}

impl CommitArtifactReport {
    pub(crate) fn new(
        logical_name: String,
        source: SourceId,
        digest: DigestV1,
        bytes: u64,
    ) -> Self {
        Self {
            logical_name,
            source,
            digest,
            bytes,
        }
    }

    #[must_use]
    pub fn logical_name(&self) -> &str {
        &self.logical_name
    }

    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn digest(&self) -> DigestV1 {
        self.digest
    }

    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Canonical, idempotency-keyed result of one durable publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitReport {
    version: u8,
    transaction: TransactionId,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    committed_revision: WorkspaceRevision,
    plan_digest: DigestV1,
    atomicity: CommitAtomicity,
    artifacts: Vec<CommitArtifactReport>,
    changes: ChangeSet,
    recovery: RecoveryLocator,
}

impl CommitReport {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        transaction: TransactionId,
        workspace_id: WorkspaceId,
        base_revision: WorkspaceRevision,
        committed_revision: WorkspaceRevision,
        plan_digest: DigestV1,
        atomicity: CommitAtomicity,
        artifacts: Vec<CommitArtifactReport>,
        changes: ChangeSet,
        recovery: RecoveryLocator,
    ) -> Self {
        Self {
            version: COMMIT_REPORT_VERSION,
            transaction,
            workspace_id,
            base_revision,
            committed_revision,
            plan_digest,
            atomicity,
            artifacts,
            changes,
            recovery,
        }
    }

    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    #[must_use]
    pub const fn transaction(&self) -> TransactionId {
        self.transaction
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
    pub const fn committed_revision(&self) -> WorkspaceRevision {
        self.committed_revision
    }

    #[must_use]
    pub const fn plan_digest(&self) -> DigestV1 {
        self.plan_digest
    }

    #[must_use]
    pub const fn atomicity(&self) -> CommitAtomicity {
        self.atomicity
    }

    #[must_use]
    pub fn artifacts(&self) -> &[CommitArtifactReport] {
        &self.artifacts
    }

    #[must_use]
    pub const fn changes(&self) -> &ChangeSet {
        &self.changes
    }

    #[must_use]
    pub const fn recovery(&self) -> &RecoveryLocator {
        &self.recovery
    }

    pub(crate) fn validate(&self) -> Result<(), CommitContractError> {
        if self.version != COMMIT_REPORT_VERSION {
            return Err(CommitContractError::UnsupportedVersion(self.version));
        }
        if self.changes.transaction() != self.transaction {
            return Err(CommitContractError::TransactionMismatch);
        }
        if self.changes.workspace() != self.workspace_id {
            return Err(CommitContractError::WorkspaceMismatch);
        }
        if self.changes.from_revision() != self.base_revision
            || self.changes.to_revision() != self.committed_revision
        {
            return Err(CommitContractError::RevisionMismatch);
        }
        if self.recovery.transaction != self.transaction {
            return Err(CommitContractError::RecoveryTransactionMismatch);
        }
        if self.artifacts.is_empty() {
            return Err(CommitContractError::EmptyArtifactSet);
        }
        for pair in self.artifacts.windows(2) {
            if pair[0].logical_name >= pair[1].logical_name {
                return Err(CommitContractError::ArtifactOrder);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct PreparedPublication {
    ordinal: u32,
    logical_name: String,
    source: SourceId,
    artifact: ArtifactHandle,
    target: PathBuf,
    relative_target: String,
    filesystem_anchor: PathBuf,
    destination_parent_identity: DirectoryIdentity,
    expected: DestinationState,
    expected_identity: Option<FileIdentity>,
    staged_identity: Option<FileIdentity>,
    digest: DigestV1,
    bytes: u64,
}

/// Absolute paths needed after the journal becomes durable.
///
/// These paths are deliberately constructed and budgeted before the manifest
/// is installed. The publication loop may then cross only durable I/O seams.
#[derive(Debug)]
struct PublicationExecution {
    ordinal: u32,
    stage: PathBuf,
    backup: Option<PathBuf>,
    security_metadata: Option<SecurityMetadataCopyReservation>,
}

struct CommitPreflight {
    publications: Vec<PreparedPublication>,
    atomicity: CommitAtomicity,
    transaction: TransactionId,
    changes: ChangeSet,
    artifacts: Vec<CommitArtifactReport>,
    recovery_baseline: JournalBaseline,
}

fn preflight_commit(
    prepared: &PreparedChange,
    target: &PublicationTarget,
    budget: &mut AssetLoadBudget,
) -> Result<CommitPreflight, CommitPreflightError> {
    let report = prepared.report();
    if report.base_revision() == report.prepared_revision() {
        return Err(CommitPreflightError::NoEffect);
    }

    let core = prepared.state().core();
    let publication_root_count = core
        .source_bindings()
        .iter()
        .filter(|binding| binding.is_publication_root())
        .count();
    let mut publication_roots =
        budgeted_vec(publication_root_count, "commit publication roots", budget)?;
    for binding in core
        .source_bindings()
        .iter()
        .filter(|binding| binding.is_publication_root())
    {
        publication_roots.push(binding);
    }
    publication_roots.sort_unstable_by_key(|binding| binding.artifact().ordinal());
    if publication_roots
        .windows(2)
        .any(|pair| pair[0].artifact() == pair[1].artifact())
    {
        return Err(CommitPreflightError::Ownership(
            "two publication roots claim the same artifact".to_owned(),
        ));
    }

    let proofs = prepared.destination_proofs().bindings();
    if proofs.len() != prepared.artifacts().len()
        || publication_roots.len() != prepared.artifacts().len()
    {
        return Err(CommitPreflightError::Ownership(
            "artifact, source, and destination counts disagree".to_owned(),
        ));
    }
    let mut publications = budgeted_vec(
        prepared.artifacts().len(),
        "commit publication bindings",
        budget,
    )?;
    for output in prepared.artifacts().outputs() {
        let binding_index = publication_roots
            .binary_search_by_key(&output.handle().ordinal(), |binding| {
                binding.artifact().ordinal()
            })
            .map_err(|_| {
                CommitPreflightError::Ownership(format!(
                    "output {} has no publication source",
                    output.name()
                ))
            })?;
        let binding = publication_roots[binding_index];
        if binding.artifact() != output.handle() {
            return Err(CommitPreflightError::Ownership(
                "publication source belongs to another artifact graph".to_owned(),
            ));
        }
        let proof_index = proofs
            .binary_search_by(|proof| proof.output_name().cmp(output.name().as_str()))
            .map_err(|_| {
                CommitPreflightError::Ownership(format!(
                    "output {} has no destination proof",
                    output.name()
                ))
            })?;
        let proof = &proofs[proof_index];
        let relative_target = relative_target(&target.root, proof.target(), budget)?;
        let logical_name =
            budgeted_string_copy(output.name().as_str(), "commit logical name", budget)?;
        let target_path = budgeted_path_copy(proof.target(), "commit target path", budget)?;
        let filesystem_anchor = budgeted_path_copy(
            proof.filesystem_anchor(),
            "commit filesystem anchor",
            budget,
        )?;
        let expected_identity = proof
            .existing_file_identity()
            .map(FileIdentity::from_physical);
        let destination_parent_identity =
            DirectoryIdentity::from_physical(proof.destination_parent_identity());
        let ordinal = u32::try_from(publications.len()).map_err(|_| {
            CommitPreflightError::Budget(BudgetError::ArithmeticOverflow {
                resource: "commit publication ordinal",
            })
        })?;
        publications.push(PreparedPublication {
            ordinal,
            logical_name,
            source: binding.source(),
            artifact: output.handle(),
            target: target_path,
            relative_target,
            filesystem_anchor,
            destination_parent_identity,
            expected: proof.expected(),
            expected_identity,
            staged_identity: None,
            digest: output.artifact().digest(),
            bytes: output.artifact().len(),
        });
    }
    publications.sort_unstable_by(|left, right| left.logical_name.cmp(&right.logical_name));
    for (ordinal, publication) in publications.iter_mut().enumerate() {
        publication.ordinal = u32::try_from(ordinal).map_err(|_| {
            CommitPreflightError::Budget(BudgetError::ArithmeticOverflow {
                resource: "commit publication ordinal",
            })
        })?;
    }

    let mut changed_sources =
        budgeted_vec(report.sources().len(), "commit changed sources", budget)?;
    for source in report.sources() {
        if source.base_fingerprint() != Some(source.prepared_fingerprint()) {
            changed_sources.push(source.source_id());
        }
    }
    changed_sources.sort_unstable();
    changed_sources.dedup();
    let mut changed_objects = budgeted_vec(
        prepared.changed_objects().len(),
        "commit changed objects",
        budget,
    )?;
    for object in prepared.changed_objects() {
        if changed_sources.binary_search(&object.source()).is_err() {
            continue;
        }
        let retained = usize_to_u64(
            object.retained_clone_bytes(),
            "commit changed object identity",
        )?;
        budget.check_bytes(retained)?;
        let object = object.clone();
        budget.consume_bytes(retained)?;
        changed_objects.push(object);
    }
    let identity_remaps = Vec::new();
    let recovery_baseline = plan_recovery_baseline(prepared, &publications, budget)?;
    let root = target
        .root
        .to_str()
        .ok_or_else(|| CommitPreflightError::UnsupportedPathEncoding(target.root.to_path_buf()))?;
    let mut output_seeds = budgeted_vec(
        publications.len(),
        "commit transaction output seeds",
        budget,
    )?;
    for publication in &publications {
        let (expected, expected_digest) = match publication.expected {
            DestinationState::Existing(fingerprint) => (
                JournalExpectedDestination::Existing,
                Some(fingerprint.digest()),
            ),
            DestinationState::Absent => (JournalExpectedDestination::Absent, None),
            DestinationState::Directory
            | DestinationState::SymbolicLink
            | DestinationState::Other => unreachable!(
                "prepare cannot retain an unsupported destination state as an expectation"
            ),
        };
        output_seeds.push(JournalTransactionOutputSeed {
            ordinal: publication.ordinal,
            logical_name: &publication.logical_name,
            source: publication.source,
            relative_target: &publication.relative_target,
            expected,
            expected_digest,
            expected_identity: publication.expected_identity.as_ref(),
            destination_parent_identity: &publication.destination_parent_identity,
            digest: publication.digest,
            bytes: publication.bytes,
        });
    }
    let seed = JournalTransactionSeed {
        version: 1,
        workspace: report.workspace_id(),
        base_revision: report.base_revision(),
        committed_revision: report.prepared_revision(),
        plan_digest: report.plan_digest(),
        atomicity: CommitAtomicity::PerArtifactRecoverable,
        containment_root: root,
        containment_root_identity: target.identity(),
        outputs: &output_seeds,
        changed_sources: &changed_sources,
        changed_objects: &changed_objects,
        identity_remaps: &identity_remaps,
        baseline: &recovery_baseline,
    };
    let transaction = transaction_id_from_seed(&seed, budget).map_err(|error| match error {
        JournalError::Budget(error) => CommitPreflightError::Budget(error),
        error => CommitPreflightError::Encoding(error.to_string()),
    })?;
    let changes = ChangeSet::new(
        transaction,
        report.workspace_id(),
        report.base_revision(),
        report.prepared_revision(),
        changed_sources,
        changed_objects,
        identity_remaps,
    )
    .map_err(|error| CommitPreflightError::ChangeSet(error.to_string()))?;
    let mut artifacts = budgeted_vec(publications.len(), "commit artifact reports", budget)?;
    for publication in &publications {
        artifacts.push(CommitArtifactReport::new(
            budgeted_string_copy(
                &publication.logical_name,
                "commit artifact report logical name",
                budget,
            )?,
            publication.source,
            publication.digest,
            publication.bytes,
        ));
    }
    Ok(CommitPreflight {
        publications,
        atomicity: CommitAtomicity::PerArtifactRecoverable,
        transaction,
        changes,
        artifacts,
        recovery_baseline,
    })
}

fn relative_target(
    root: &Path,
    target: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<String, CommitPreflightError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| CommitPreflightError::TargetEscapesRoot(target.to_path_buf()))?;
    let mut requested = 0_usize;
    for (component_count, component) in relative.components().enumerate() {
        let std::path::Component::Normal(component) = component else {
            return Err(CommitPreflightError::TargetEscapesRoot(
                target.to_path_buf(),
            ));
        };
        let component = component
            .to_str()
            .ok_or_else(|| CommitPreflightError::UnsupportedPathEncoding(target.to_path_buf()))?;
        requested = requested
            .checked_add(component.len())
            .and_then(|length| length.checked_add(usize::from(component_count != 0)))
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "commit relative target",
            })?;
    }
    budget.check_bytes(usize_to_u64(requested, "commit relative target")?)?;
    let mut encoded = String::new();
    encoded
        .try_reserve_exact(requested)
        .map_err(|error| CommitPreflightError::Allocation {
            resource: "commit relative target",
            requested,
            message: error.to_string(),
        })?;
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            unreachable!("relative target components were validated in the sizing pass");
        };
        let component = component
            .to_str()
            .expect("relative target encoding was validated in the sizing pass");
        if !encoded.is_empty() {
            encoded.push('/');
        }
        encoded.push_str(component);
    }
    budget.consume_bytes(usize_to_u64(encoded.capacity(), "commit relative target")?)?;
    let portability_key = slash_key(&encoded, budget).map_err(map_portable_path_error)?;
    let validation_peak = encoded.len().checked_add(portability_key.len()).ok_or(
        BudgetError::ArithmeticOverflow {
            resource: "commit relative target validation",
        },
    )?;
    budget.check_bytes(usize_to_u64(
        validation_peak,
        "commit relative target validation",
    )?)?;
    let validated = LogicalArtifactName::new(&encoded)
        .map_err(|error| CommitPreflightError::InvalidRelativeTarget(error.to_string()))?;
    let retained = validated
        .retained_bytes()
        .map_err(|error| CommitPreflightError::InvalidRelativeTarget(error.to_string()))?;
    budget.consume_bytes(retained)?;
    Ok(encoded)
}

fn budgeted_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, CommitPreflightError> {
    let entries = usize_to_u64(capacity, resource)?;
    let planned = size_of::<T>()
        .checked_mul(capacity)
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_entries(entries)?;
    budget.check_bytes(usize_to_u64(planned, resource)?)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|error| CommitPreflightError::Allocation {
            resource,
            requested: capacity,
            message: error.to_string(),
        })?;
    let actual = size_of::<T>()
        .checked_mul(values.capacity())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(usize_to_u64(actual, resource)?)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(usize_to_u64(actual, resource)?)?;
    Ok(values)
}

fn budgeted_string_copy(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, CommitPreflightError> {
    budget.check_bytes(usize_to_u64(value.len(), resource)?)?;
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|error| CommitPreflightError::Allocation {
            resource,
            requested: value.len(),
            message: error.to_string(),
        })?;
    copy.push_str(value);
    budget.consume_bytes(usize_to_u64(copy.capacity(), resource)?)?;
    Ok(copy)
}

fn budgeted_path_copy(
    value: &Path,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, CommitPreflightError> {
    let requested = value.as_os_str().len();
    budget.check_bytes(usize_to_u64(requested, resource)?)?;
    let mut copy = PathBuf::new();
    copy.try_reserve_exact(requested)
        .map_err(|error| CommitPreflightError::Allocation {
            resource,
            requested,
            message: error.to_string(),
        })?;
    copy.push(value);
    budget.consume_bytes(usize_to_u64(copy.capacity(), resource)?)?;
    Ok(copy)
}

fn map_portable_path_error(error: PortablePathError) -> CommitPreflightError {
    match error {
        PortablePathError::Budget(error) => CommitPreflightError::Budget(error),
        PortablePathError::UnsupportedEncoding => CommitPreflightError::InvalidRelativeTarget(
            "relative target contains unsupported encoding".to_owned(),
        ),
        PortablePathError::Allocation { requested, message } => CommitPreflightError::Allocation {
            resource: "commit relative target portability key",
            requested,
            message,
        },
    }
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, CommitPreflightError> {
    u64::try_from(value)
        .map_err(|_| CommitPreflightError::Budget(BudgetError::ArithmeticOverflow { resource }))
}

#[derive(Debug, Error)]
enum CommitPreflightError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to allocate {requested} entries for {resource}: {message}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        message: String,
    },
    #[error("prepared change has no semantic effect")]
    NoEffect,
    #[error("prepared publication ownership is inconsistent: {0}")]
    Ownership(String),
    #[error("publication target escapes the containment root: {0:?}")]
    TargetEscapesRoot(PathBuf),
    #[error("publication target uses an unsupported path encoding: {0:?}")]
    UnsupportedPathEncoding(PathBuf),
    #[error("invalid root-relative publication target: {0}")]
    InvalidRelativeTarget(String),
    #[error("failed to encode transaction identity: {0}")]
    Encoding(String),
    #[error("failed to construct change set: {0}")]
    ChangeSet(String),
}

struct ReadyPublication {
    _root: CommitRoot,
    _journal_namespace: JournalNamespace,
    _guard: CommitGuard,
    publications: Vec<PreparedPublication>,
    execution: Vec<PublicationExecution>,
    baseline: PreparedBaseline,
    journal: Journal,
    event_plan: JournalEventPlan,
    report: CommitReport,
}

#[derive(Debug)]
struct PreparedJournalDirectories {
    identities: JournalDirectoryIdentities,
    stage: JournalDirectory,
    backup: JournalDirectory,
    baseline: JournalDirectory,
}

impl AssetWorkspace {
    /// Durably publishes one exact prepared change and installs its immutable baseline.
    pub fn commit(
        &mut self,
        prepared: PreparedChange,
        target: PublicationTarget,
        budget: &mut AssetLoadBudget,
    ) -> Result<CommitReport, CommitError> {
        if prepared.report().workspace_id() != self.workspace_id() {
            return Err(CommitError::WorkspaceMismatch {
                expected: self.workspace_id(),
                actual: prepared.report().workspace_id(),
            });
        }
        if prepared.report().base_revision() != self.revision()
            || !Arc::ptr_eq(prepared.state().core().base().state(), self.state())
        {
            return Err(CommitError::StaleRevision {
                expected: prepared.report().base_revision(),
                actual: self.revision(),
            });
        }
        if prepared.report().base_revision() == prepared.report().prepared_revision() {
            return Err(CommitError::NoEffect);
        }

        let ready = match self.prepare_publication(&prepared, &target, budget) {
            Ok(ready) => ready,
            Err(PreparePublicationError::Budget(source)) => {
                return Err(CommitError::Budget {
                    source,
                    prepared: Box::new(prepared),
                });
            }
            Err(PreparePublicationError::StaleRevision { expected, actual }) => {
                return Err(CommitError::StaleRevision { expected, actual });
            }
            Err(PreparePublicationError::SourceConflict {
                source_id,
                expected,
                actual,
            }) => {
                return Err(CommitError::SourceConflict {
                    source_id,
                    expected,
                    actual,
                });
            }
            Err(PreparePublicationError::DestinationConflict {
                output,
                expected,
                actual,
            }) => {
                return Err(CommitError::DestinationConflict {
                    output,
                    expected: expected.into(),
                    actual: actual.into(),
                });
            }
            Err(PreparePublicationError::PublishBlocked(message)) => {
                return Err(CommitError::PublishBlocked { message });
            }
            Err(PreparePublicationError::RecoveryRequired { locator, message }) => {
                return Err(CommitError::RecoveryRequired { locator, message });
            }
            Err(PreparePublicationError::Retryable(message)) => {
                return Err(CommitError::Retryable {
                    message,
                    prepared: Box::new(prepared),
                });
            }
        };
        self.publish_ready(ready)
    }

    fn prepare_publication(
        &self,
        prepared: &PreparedChange,
        target: &PublicationTarget,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReadyPublication, PreparePublicationError> {
        let root = open_commit_root(target.root(), target.identity())
            .map_err(|error| map_prejournal_io("open publication target root", error))?;
        let guard = CommitGuard::acquire_with_root(&root)
            .map_err(|error| map_prejournal_io("acquire publication guard", error))?;
        target
            .verify_current()
            .map_err(|error| map_prejournal_io("reverify publication target root", error))?;
        if prepared.report().base_revision() != self.revision()
            || !Arc::ptr_eq(prepared.state().core().base().state(), self.state())
        {
            return Err(PreparePublicationError::StaleRevision {
                expected: prepared.report().base_revision(),
                actual: self.revision(),
            });
        }
        prepared
            .source_proofs()
            .revalidate(budget)
            .map_err(map_source_proof_error)?;
        prepared
            .destination_proofs()
            .revalidate(budget)
            .map_err(map_destination_proof_error)?;
        let preflight = preflight_commit(prepared, target, budget).map_err(map_preflight_error)?;
        let CommitPreflight {
            mut publications,
            atomicity,
            transaction,
            changes,
            artifacts,
            recovery_baseline,
        } = preflight;
        validate_publication_hardlinks(&publications)?;
        let layout = JournalLayout::new_budgeted(
            target.root(),
            transaction,
            target.identity().clone(),
            budget,
        )
        .map_err(map_journal_layout_prepare_error)?;
        let locator = RecoveryLocator::new(
            layout
                .directory_path_budgeted(budget)
                .map_err(map_journal_layout_prepare_error)?,
            transaction,
            layout.root_identity().clone(),
        );
        let mut execution = publication_execution_plan(&layout, &publications, budget)
            .map_err(map_preflight_error)?;
        let prepare_report = prepared.report();
        let report = CommitReport::new(
            transaction,
            prepare_report.workspace_id(),
            prepare_report.base_revision(),
            prepare_report.prepared_revision(),
            prepare_report.plan_digest(),
            atomicity,
            artifacts,
            changes,
            locator.clone(),
        );
        report
            .validate()
            .map_err(|error| PreparePublicationError::PublishBlocked(error.to_string()))?;
        let journal_namespace = prepare_recovery_namespace(&root)?;
        let access = journal_access(&root, &journal_namespace);
        match open_journal_regular(&access, layout.preparation_path()) {
            Ok(_) => {
                return Err(PreparePublicationError::RecoveryRequired {
                    locator,
                    message: "this transaction already has durable recovery evidence".to_owned(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(PreparePublicationError::RecoveryRequired {
                    locator,
                    message: format!("inspect durable preparation evidence: {error}"),
                });
            }
        }
        match open_journal_directory(&access, layout.directory()) {
            Ok(directory) => {
                match open_journal_regular_in_directory(&directory, layout.manifest_path()) {
                    Ok(_) => {
                        return Err(PreparePublicationError::RecoveryRequired {
                            locator,
                            message: "this transaction already has durable recovery evidence"
                                .to_owned(),
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Err(PreparePublicationError::PublishBlocked(
                        "an unowned transaction directory exists without a durable preparation record"
                            .to_owned(),
                    ));
                    }
                    Err(error) => {
                        return Err(PreparePublicationError::RecoveryRequired {
                            locator,
                            message: format!("inspect canonical manifest evidence: {error}"),
                        });
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(map_prejournal_io(
                    "inspect recovery transaction directory",
                    error,
                ));
            }
        }
        match JournalPreparation::acknowledge_matching_rollback_in_access(
            &layout, &report, &access, budget,
        ) {
            Ok(()) => {}
            Err(JournalError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(JournalError::Budget(source)) => {
                return Err(PreparePublicationError::Budget(source));
            }
            Err(JournalError::Io(error)) => {
                return Err(map_prejournal_io("acknowledge terminal rollback", error));
            }
            Err(error) => {
                return Err(PreparePublicationError::RecoveryRequired {
                    locator,
                    message: error.to_string(),
                });
            }
        }
        let preparation_outputs =
            journal_preparation_outputs(&publications, budget).map_err(map_preflight_error)?;
        recovery::cleanup_orphaned_preparation_attempts(&layout, &access, budget)
            .map_err(map_orphaned_preparation_cleanup_error)?;
        let preparation_temporary =
            preparation_temporary_path(&layout, budget).map_err(map_preflight_error)?;
        let preparation = match JournalPreparation::install_in_access(
            &layout,
            &report,
            &preparation_outputs,
            &recovery_baseline,
            &preparation_temporary,
            &access,
            budget,
        ) {
            Ok(preparation) => preparation,
            Err(error) if error.preparation_installed() => {
                return Err(PreparePublicationError::RecoveryRequired {
                    locator,
                    message: error.to_string(),
                });
            }
            Err(error) => {
                return Err(map_unpublished_journal_prepare_error(error.into_source()));
            }
        };
        #[cfg(test)]
        test_crash_failpoint("preparation_installed");
        let directories =
            match prepare_empty_transaction_directory(&layout, &preparation, &access, budget) {
                Ok(directories) => directories,
                Err(error) => {
                    return Err(cleanup_prejournal_error(
                        &layout,
                        &access,
                        error,
                        "private recovery namespace creation failed",
                        budget,
                    ));
                }
            };
        #[cfg(test)]
        test_crash_failpoint("private_directories_synced");

        let result = (|| {
            for publication in &publications {
                ensure_journal_directory_same_filesystem(
                    &directories.stage,
                    &publication.filesystem_anchor,
                )
                    .and_then(|()| {
                        ensure_journal_directory_same_filesystem(
                            &directories.backup,
                            &publication.filesystem_anchor,
                        )
                    })
                    .map_err(|error| {
                        PreparePublicationError::PublishBlocked(format!(
                            "publication staging and destination are not on one supported filesystem: {error}"
                        ))
                    })?;
            }
            let mut images = MaterializedImages::new(prepared.artifacts(), budget)
                .map_err(map_baseline_prepare_error)?;
            for (publication, execution) in publications.iter_mut().zip(&execution) {
                debug_assert_eq!(publication.ordinal, execution.ordinal);
                let path = &execution.stage;
                let mut file = create_journal_regular_in_directory(&directories.stage, path)
                    .map_err(|error| map_prejournal_io("create staged artifact", error))?;
                images
                    .stream_and_materialize(
                        prepared.artifacts(),
                        publication.artifact,
                        &mut file,
                        budget,
                    )
                    .map_err(map_baseline_prepare_error)?;
                file.flush()
                    .map_err(|error| map_prejournal_io("flush staged artifact", error))?;
                file.sync_all()
                    .map_err(|error| map_prejournal_io("sync staged artifact", error))?;
                let staged_identity = opened_file_identity(&file).map_err(|error| {
                    map_prejournal_io("capture staged artifact identity", error)
                })?;
                drop(file);
                if let Some(expected_identity) = publication.expected_identity.as_ref() {
                    copy_security_metadata_external_to_journal_directory(
                        &publication.target,
                        &directories.stage,
                        path,
                        expected_identity,
                        &publication.destination_parent_identity,
                        &staged_identity,
                        budget,
                    )
                    .map_err(map_security_metadata_prepare_error)?;
                }
                publication.staged_identity = Some(staged_identity);
                sync_journal_directory(&directories.stage)
                    .map_err(|error| map_prejournal_io("sync staging directory", error))?;
                #[cfg(test)]
                test_crash_artifact_failpoint("staged_artifact_synced", publication.ordinal);
            }
            let baseline = baseline::build(prepared, self.binary_adapter(), &mut images, budget)
                .map_err(map_baseline_prepare_error)?;
            write_recovery_baseline(
                prepared,
                &recovery_baseline,
                &images,
                &layout,
                &directories.baseline,
                budget,
            )
            .map_err(map_recovery_baseline_prepare_error)?;
            #[cfg(test)]
            test_crash_failpoint("recovery_baseline_synced");
            Ok::<PreparedBaseline, PreparePublicationError>(baseline)
        })();
        let baseline = match result {
            Ok(result) => result,
            Err(error) => {
                drop(directories);
                return Err(cleanup_prejournal_error(
                    &layout,
                    &access,
                    error,
                    "publication staging failed",
                    budget,
                ));
            }
        };
        let directory_identities = directories.identities.clone();
        drop(directories);
        let final_validation = prepared
            .source_proofs()
            .revalidate(budget)
            .map_err(map_source_proof_error)
            .and_then(|()| {
                prepared
                    .destination_proofs()
                    .revalidate(budget)
                    .map_err(map_destination_proof_error)
            });
        if let Err(error) = final_validation {
            return Err(cleanup_prejournal_error(
                &layout,
                &access,
                error,
                "final publication validation failed",
                budget,
            ));
        }
        if let Err(error) = validate_publication_hardlinks(&publications) {
            return Err(cleanup_prejournal_error(
                &layout,
                &access,
                error,
                "final hard-link validation failed",
                budget,
            ));
        }
        if let Err(error) =
            reserve_publication_security_metadata(&publications, &mut execution, budget)
        {
            return Err(cleanup_prejournal_error(
                &layout,
                &access,
                PreparePublicationError::Budget(error),
                "security metadata reservation failed",
                budget,
            ));
        }
        let artifacts = match journal_artifacts(&publications, &report, budget) {
            Ok(artifacts) => artifacts,
            Err(error) => {
                return Err(cleanup_prejournal_error(
                    &layout,
                    &access,
                    map_preflight_error(error),
                    "journal artifact construction failed",
                    budget,
                ));
            }
        };
        let manifest = match JournalManifest::new(
            &report,
            layout.root_identity().clone(),
            directory_identities,
            artifacts,
            recovery_baseline,
            budget,
        ) {
            Ok(manifest) => manifest,
            Err(error) => {
                return Err(cleanup_prejournal_error(
                    &layout,
                    &access,
                    map_unpublished_journal_prepare_error(error),
                    "journal manifest construction failed",
                    budget,
                ));
            }
        };
        let event_keys = match commit_event_keys(&publications, budget) {
            Ok(keys) => keys,
            Err(error) => {
                return Err(cleanup_prejournal_error(
                    &layout,
                    &access,
                    map_preflight_error(error),
                    "commit event planning failed",
                    budget,
                ));
            }
        };
        let (journal, event_plan) =
            match Journal::create_planned_in_access(layout, manifest, &event_keys, &access, budget)
            {
                Ok(ready) => ready,
                Err(error) if error.manifest_installed() => {
                    return Err(PreparePublicationError::RecoveryRequired {
                        locator,
                        message: error.to_string(),
                    });
                }
                Err(error) => {
                    let (layout, source) = error.into_parts();
                    return Err(cleanup_prejournal_error(
                        &layout,
                        &access,
                        map_unpublished_journal_prepare_error(source),
                        "canonical manifest was not published",
                        budget,
                    ));
                }
            };
        #[cfg(test)]
        test_crash_failpoint("manifest_installed");
        Ok(ReadyPublication {
            _root: root,
            _journal_namespace: journal_namespace,
            _guard: guard,
            publications,
            execution,
            baseline,
            journal,
            event_plan,
            report,
        })
    }

    fn publish_ready(&mut self, ready: ReadyPublication) -> Result<CommitReport, CommitError> {
        let ReadyPublication {
            _root,
            _journal_namespace,
            _guard,
            publications,
            mut execution,
            baseline,
            mut journal,
            mut event_plan,
            report,
        } = ready;
        let locator = report.recovery().clone();
        debug_assert_eq!(publications.len(), execution.len());
        journal
            .append_planned(&mut event_plan, JournalEventKey::StagingVerified)
            .and_then(|_| journal.append_planned(&mut event_plan, JournalEventKey::Journaled))
            .map_err(|error| CommitError::RecoveryRequired {
                locator: locator.clone(),
                message: error.to_string(),
            })?;

        for (publication, execution) in publications.iter().zip(&mut execution) {
            debug_assert_eq!(publication.ordinal, execution.ordinal);
            match publish_one(&mut journal, &mut event_plan, publication, execution) {
                Ok(()) => {}
                Err(PublishError::Message(message)) => {
                    return Err(CommitError::RecoveryRequired {
                        locator: locator.clone(),
                        message,
                    });
                }
            }
        }
        journal
            .append_planned(&mut event_plan, JournalEventKey::Published)
            .map_err(|error| CommitError::RecoveryRequired {
                locator: locator.clone(),
                message: error.to_string(),
            })?;
        #[cfg(test)]
        test_crash_failpoint("published");

        if prepared_revision_changed(self, &baseline)
            || !self.install_state_if_current(&baseline.expected, baseline.next)
        {
            return Err(CommitError::RecoveryRequired {
                locator,
                message:
                    "published bytes could not be installed over the expected workspace baseline"
                        .to_owned(),
            });
        }
        #[cfg(test)]
        test_crash_failpoint("baseline_cas_before_event");
        journal
            .append_planned(&mut event_plan, JournalEventKey::BaselineInstalled)
            .and_then(|_| journal.append_planned(&mut event_plan, JournalEventKey::Finalized))
            .map_err(|error| CommitError::RecoveryRequired {
                locator: locator.clone(),
                message: error.to_string(),
            })?;
        #[cfg(test)]
        test_crash_failpoint("finalized_before_response");
        event_plan
            .finish()
            .map_err(|error| CommitError::RecoveryRequired {
                locator: locator.clone(),
                message: error.to_string(),
            })?;
        Ok(report)
    }
}

fn prepare_recovery_namespace(
    root: &CommitRoot,
) -> Result<JournalNamespace, PreparePublicationError> {
    let namespace = open_journal_namespace(root)
        .map_err(|error| map_prejournal_io("open recovery namespace", error))?;
    sync_journal_namespace(root, &namespace)
        .map_err(|error| map_prejournal_io("persist recovery namespace", error))?;
    Ok(namespace)
}

fn preparation_temporary_path(
    layout: &JournalLayout,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, CommitPreflightError> {
    let version_directory = layout.directory().parent().ok_or_else(|| {
        CommitPreflightError::Ownership("recovery transaction has no version directory".to_owned())
    })?;
    let slug = layout
        .directory()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CommitPreflightError::Ownership(
                "recovery transaction has no canonical UTF-8 slug".to_owned(),
            )
        })?;
    let requested_name = slug.len().checked_add(1 + ".prepare.v2.tmp".len()).ok_or(
        BudgetError::ArithmeticOverflow {
            resource: "journal preparation temporary name",
        },
    )?;
    budget.check_bytes(usize_to_u64(
        requested_name,
        "journal preparation temporary name",
    )?)?;
    let mut name = String::new();
    name.try_reserve_exact(requested_name)
        .map_err(|error| CommitPreflightError::Allocation {
            resource: "journal preparation temporary name",
            requested: requested_name,
            message: error.to_string(),
        })?;
    write!(&mut name, ".{slug}.prepare.v2.tmp").map_err(|_| {
        CommitPreflightError::Ownership(
            "journal preparation temporary name formatting failed".to_owned(),
        )
    })?;
    if name.len() != requested_name {
        return Err(CommitPreflightError::Ownership(
            "journal preparation temporary name is not canonical".to_owned(),
        ));
    }
    budget.consume_bytes(usize_to_u64(
        name.capacity(),
        "journal preparation temporary name",
    )?)?;
    let requested_path = version_directory
        .as_os_str()
        .len()
        .checked_add(1)
        .and_then(|bytes| bytes.checked_add(name.len()))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "journal preparation temporary path",
        })?;
    budget.check_bytes(usize_to_u64(
        requested_path,
        "journal preparation temporary path",
    )?)?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(requested_path)
        .map_err(|error| CommitPreflightError::Allocation {
            resource: "journal preparation temporary path",
            requested: requested_path,
            message: error.to_string(),
        })?;
    path.push(version_directory);
    path.push(name);
    budget.consume_bytes(usize_to_u64(
        path.capacity(),
        "journal preparation temporary path",
    )?)?;
    Ok(path)
}

fn prepare_empty_transaction_directory(
    layout: &JournalLayout,
    preparation: &OpenedJournalPreparation,
    access: &JournalAccess<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedJournalDirectories, PreparePublicationError> {
    preparation
        .revalidate_in_access(layout, access, budget)
        .map_err(map_unpublished_journal_prepare_error)?;
    let transaction = match create_journal_directory(access, layout.directory()) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(PreparePublicationError::RecoveryRequired {
                locator: RecoveryLocator::new(
                    layout.directory().to_path_buf(),
                    layout.transaction(),
                    layout.root_identity().clone(),
                ),
                message: "this prepared transaction already has private recovery state".to_owned(),
            });
        }
        Err(error) => {
            return Err(map_prejournal_io(
                "create exclusive recovery transaction",
                error,
            ));
        }
    };
    let transaction_identity = journal_directory_identity(&transaction)
        .map_err(|error| map_prejournal_io("capture recovery transaction identity", error))?;
    #[cfg(test)]
    test_crash_failpoint("transaction_directory_installed");
    let events = create_journal_directory_in_directory(&transaction, layout.events_directory())
        .map_err(|error| {
            PreparePublicationError::PublishBlocked(format!(
                "exclusive recovery transaction was retained after events directory creation failed: {error}"
            ))
        })?;
    let events_identity = journal_directory_identity(&events)
        .map_err(|error| map_prejournal_io("capture recovery events identity", error))?;
    let stage = create_journal_directory_in_directory(&transaction, layout.stage_directory())
        .map_err(|error| {
            PreparePublicationError::PublishBlocked(format!(
                "exclusive recovery transaction was retained after stage directory creation failed: {error}"
            ))
        })?;
    let stage_identity = journal_directory_identity(&stage)
        .map_err(|error| map_prejournal_io("capture recovery stage identity", error))?;
    let backup = create_journal_directory_in_directory(&transaction, layout.backup_directory())
        .map_err(|error| {
            PreparePublicationError::PublishBlocked(format!(
                "exclusive recovery transaction was retained after backup directory creation failed: {error}"
            ))
        })?;
    let backup_identity = journal_directory_identity(&backup)
        .map_err(|error| map_prejournal_io("capture recovery backup identity", error))?;
    let baseline = create_journal_directory_in_directory(&transaction, layout.baseline_directory())
        .map_err(|error| {
            PreparePublicationError::PublishBlocked(format!(
                "exclusive recovery transaction was retained after baseline directory creation failed: {error}"
            ))
        })?;
    let baseline_identity = journal_directory_identity(&baseline)
        .map_err(|error| map_prejournal_io("capture recovery baseline identity", error))?;
    let identities = JournalDirectoryIdentities::new(
        transaction_identity,
        events_identity,
        stage_identity,
        backup_identity,
        baseline_identity,
    );
    let durability = sync_journal_directory(&events)
        .and_then(|()| sync_journal_directory(&stage))
        .and_then(|()| sync_journal_directory(&backup))
        .and_then(|()| sync_journal_directory(&baseline))
        .and_then(|()| sync_journal_directory(&transaction))
        .and_then(|()| sync_journal_access(access));
    if let Err(error) = durability {
        return Err(map_prejournal_io(
            "persist private recovery namespace",
            error,
        ));
    }
    Ok(PreparedJournalDirectories {
        identities,
        stage,
        backup,
        baseline,
    })
}

fn cleanup_prejournal_error(
    layout: &JournalLayout,
    access: &JournalAccess<'_>,
    error: PreparePublicationError,
    context: &'static str,
    operation_budget: &mut AssetLoadBudget,
) -> PreparePublicationError {
    let cleanup = if matches!(&error, PreparePublicationError::Budget(_)) {
        recovery::cleanup_prepared_transaction_after_budget_exhaustion(layout, access)
    } else {
        recovery::cleanup_prepared_transaction(layout, access, operation_budget)
    };
    let Err(cleanup) = cleanup else {
        return error;
    };
    let original = error.to_string();
    PreparePublicationError::RecoveryRequired {
        locator: RecoveryLocator::new(
            layout.directory().to_path_buf(),
            layout.transaction(),
            layout.root_identity().clone(),
        ),
        message: format!(
            "{context} ({original}); prepared transaction cleanup also failed: {cleanup}"
        ),
    }
}

fn reserve_publication_security_metadata(
    publications: &[PreparedPublication],
    execution: &mut [PublicationExecution],
    budget: &mut AssetLoadBudget,
) -> Result<(), BudgetError> {
    debug_assert_eq!(publications.len(), execution.len());
    for (publication, execution) in publications.iter().zip(execution) {
        debug_assert_eq!(publication.ordinal, execution.ordinal);
        if matches!(publication.expected, DestinationState::Existing(_)) {
            execution.security_metadata = Some(reserve_security_metadata_copy(budget)?);
        }
    }
    Ok(())
}

fn map_orphaned_preparation_cleanup_error(
    error: recovery::PremanifestCleanupError,
) -> PreparePublicationError {
    match error {
        recovery::PremanifestCleanupError::Budget(error) => PreparePublicationError::Budget(error),
        recovery::PremanifestCleanupError::Io(error) => {
            map_prejournal_io("clean orphaned preparation attempts", error)
        }
        error => PreparePublicationError::PublishBlocked(error.to_string()),
    }
}

fn publication_execution_plan(
    layout: &JournalLayout,
    publications: &[PreparedPublication],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<PublicationExecution>, CommitPreflightError> {
    let mut execution = budgeted_vec(
        publications.len(),
        "commit publication execution paths",
        budget,
    )?;
    for publication in publications {
        let stage = budgeted_ordinal_child_path(
            layout.stage_directory(),
            publication.ordinal,
            ".stage",
            "commit staging execution path",
            budget,
        )?;
        let backup = if matches!(publication.expected, DestinationState::Existing(_)) {
            Some(budgeted_ordinal_child_path(
                layout.backup_directory(),
                publication.ordinal,
                ".backup",
                "commit backup execution path",
                budget,
            )?)
        } else {
            None
        };
        execution.push(PublicationExecution {
            ordinal: publication.ordinal,
            stage,
            backup,
            security_metadata: None,
        });
    }
    Ok(execution)
}

fn budgeted_ordinal_child_path(
    directory: &Path,
    ordinal: u32,
    suffix: &'static str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, CommitPreflightError> {
    let filename = budgeted_ordinal_filename(ordinal, suffix, resource, budget)?;
    let requested = directory
        .as_os_str()
        .len()
        .checked_add(filename.len())
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(usize_to_u64(requested, resource)?)?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(requested)
        .map_err(|error| CommitPreflightError::Allocation {
            resource,
            requested,
            message: error.to_string(),
        })?;
    path.push(directory);
    path.push(filename);
    budget.consume_bytes(usize_to_u64(path.capacity(), resource)?)?;
    Ok(path)
}

fn budgeted_ordinal_filename(
    ordinal: u32,
    suffix: &'static str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, CommitPreflightError> {
    let requested = 8_usize
        .checked_add(suffix.len())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(usize_to_u64(requested, resource)?)?;
    let mut filename = String::new();
    filename
        .try_reserve_exact(requested)
        .map_err(|error| CommitPreflightError::Allocation {
            resource,
            requested,
            message: error.to_string(),
        })?;
    write!(&mut filename, "{ordinal:08}{suffix}").map_err(|_| {
        CommitPreflightError::Ownership("ordinal file name formatting failed".to_owned())
    })?;
    if filename.len() != requested {
        return Err(CommitPreflightError::Ownership(
            "ordinal file name has a non-canonical length".to_owned(),
        ));
    }
    budget.consume_bytes(usize_to_u64(filename.capacity(), resource)?)?;
    Ok(filename)
}

fn recovery_baseline_image_paths(
    layout: &JournalLayout,
    index: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(JournalPath, PathBuf), RecoveryBaselineWriteError> {
    let ordinal = u32::try_from(index).map_err(|_| {
        RecoveryBaselineWriteError::Budget(BudgetError::ArithmeticOverflow {
            resource: "recovery baseline image ordinal",
        })
    })?;
    let relative = ordinal_journal_path(
        "baseline/",
        ordinal,
        ".image",
        "recovery baseline image journal path",
        budget,
    )
    .map_err(map_recovery_baseline_path_error)?;
    let path = budgeted_ordinal_child_path(
        layout.baseline_directory(),
        ordinal,
        ".image",
        "recovery baseline image path",
        budget,
    )
    .map_err(map_recovery_baseline_path_error)?;
    Ok((relative, path))
}

fn map_recovery_baseline_path_error(error: CommitPreflightError) -> RecoveryBaselineWriteError {
    match error {
        CommitPreflightError::Budget(error) => RecoveryBaselineWriteError::Budget(error),
        CommitPreflightError::Allocation { message, .. } => {
            RecoveryBaselineWriteError::Allocation(message)
        }
        error => RecoveryBaselineWriteError::Invariant(error.to_string()),
    }
}

fn plan_recovery_baseline(
    prepared: &PreparedChange,
    publications: &[PreparedPublication],
    budget: &mut AssetLoadBudget,
) -> Result<JournalBaseline, CommitPreflightError> {
    let core = prepared.state().core();
    let bindings = core.source_bindings();
    let base_catalog = core.base().state().catalog();
    let candidate_catalog = core.catalog();
    let mut sources = budgeted_vec(bindings.len(), "recovery baseline sources", budget)?;
    for (index, binding) in bindings.iter().enumerate() {
        let image = if binding.is_publication_root() {
            let publication = publications
                .iter()
                .find(|publication| publication.artifact == binding.artifact())
                .ok_or_else(|| {
                    CommitPreflightError::Ownership(
                        "publication root has no output ordinal".to_owned(),
                    )
                })?;
            JournalBaselineImage::Published {
                artifact: publication.ordinal,
            }
        } else {
            let artifact = prepared
                .artifacts()
                .artifact(binding.artifact())
                .map_err(|error| CommitPreflightError::Ownership(error.to_string()))?;
            if artifact.digest() != binding.fingerprint().digest() {
                return Err(CommitPreflightError::Ownership(
                    "nested source fingerprint disagrees with its proof image".to_owned(),
                ));
            }
            let ordinal = u32::try_from(index).map_err(|_| {
                CommitPreflightError::Budget(BudgetError::ArithmeticOverflow {
                    resource: "recovery baseline image ordinal",
                })
            })?;
            JournalBaselineImage::Blob {
                path: ordinal_journal_path(
                    "baseline/",
                    ordinal,
                    ".image",
                    "recovery baseline image journal path",
                    budget,
                )?,
                digest: binding.fingerprint().digest(),
                bytes: artifact.len(),
            }
        };
        let catalog = if base_catalog.contains(binding.source()) {
            let base_fingerprint = base_catalog
                .fingerprint(binding.source())
                .map_err(|error| CommitPreflightError::Ownership(error.to_string()))?;
            JournalCatalogAction::Existing { base_fingerprint }
        } else {
            let parent = candidate_catalog
                .parent(binding.source())
                .map_err(|error| CommitPreflightError::Ownership(error.to_string()))?
                .ok_or_else(|| {
                    CommitPreflightError::Ownership("new baseline source has no parent".to_owned())
                })?;
            let locator = candidate_catalog
                .source_locator(binding.source())
                .map_err(|error| CommitPreflightError::Ownership(error.to_string()))?;
            let step = locator.members().last().ok_or_else(|| {
                CommitPreflightError::Ownership("new baseline source has no member step".to_owned())
            })?;
            let member_bytes = usize_to_u64(
                step.member().retained_clone_bytes(),
                "recovery source member allocation",
            )?;
            budget.check_bytes(member_bytes)?;
            let member = step.member().clone();
            budget.consume_bytes(member_bytes)?;
            match step.container() {
                ContainmentKind::Companion => JournalCatalogAction::AddCompanion { parent, member },
                ContainmentKind::Archive | ContainmentKind::Bundle | ContainmentKind::WebFile => {
                    JournalCatalogAction::AddContainedSidecar { parent, member }
                }
            }
        };
        sources.push(JournalBaselineSource::new(
            binding.source(),
            binding.fingerprint(),
            catalog,
            image,
        ));
    }
    JournalBaseline::from_sorted(sources, prepared.report().workspace_id())
        .map_err(map_journal_preflight_error)
}

fn write_recovery_baseline(
    prepared: &PreparedChange,
    planned: &JournalBaseline,
    images: &MaterializedImages,
    layout: &JournalLayout,
    baseline_directory: &JournalDirectory,
    budget: &mut AssetLoadBudget,
) -> Result<(), RecoveryBaselineWriteError> {
    let core = prepared.state().core();
    let bindings = core.source_bindings();
    if bindings.len() != planned.sources().len() {
        return Err(RecoveryBaselineWriteError::Invariant(
            "recovery baseline plan does not cover every prepared source".to_owned(),
        ));
    }
    for (index, (binding, source)) in bindings.iter().zip(planned.sources()).enumerate() {
        if source.source() != binding.source() || source.fingerprint() != binding.fingerprint() {
            return Err(RecoveryBaselineWriteError::Invariant(
                "recovery baseline source changed after planning".to_owned(),
            ));
        }
        let JournalBaselineImage::Blob {
            path: expected_relative,
            digest,
            bytes: expected_bytes,
        } = source.image()
        else {
            if !binding.is_publication_root() {
                return Err(RecoveryBaselineWriteError::Invariant(
                    "nested recovery source has no retained blob".to_owned(),
                ));
            }
            continue;
        };
        if binding.is_publication_root() {
            return Err(RecoveryBaselineWriteError::Invariant(
                "publication root unexpectedly retained a baseline blob".to_owned(),
            ));
        }
        let image = images.get(binding.artifact()).ok_or_else(|| {
            RecoveryBaselineWriteError::Invariant(
                "nested source image was not materialized".to_owned(),
            )
        })?;
        let actual_bytes =
            u64::try_from(image.len()).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "nested recovery image length",
            })?;
        if actual_bytes != *expected_bytes || DigestV1::hash_bytes(image) != *digest {
            return Err(RecoveryBaselineWriteError::Invariant(
                "nested recovery image disagrees with its plan".to_owned(),
            ));
        }
        let (relative, path) = recovery_baseline_image_paths(layout, index, budget)?;
        if &relative != expected_relative {
            return Err(RecoveryBaselineWriteError::Invariant(
                "nested recovery image path changed after planning".to_owned(),
            ));
        }
        let mut file = create_journal_regular_in_directory(baseline_directory, &path)?;
        file.write_all(image)?;
        file.sync_all()?;
    }
    sync_journal_directory(baseline_directory)?;
    Ok(())
}

#[derive(Debug, Error)]
enum RecoveryBaselineWriteError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("recovery baseline invariant failed: {0}")]
    Invariant(String),
    #[error("recovery baseline allocation failed: {0}")]
    Allocation(String),
}

fn journal_preparation_outputs(
    publications: &[PreparedPublication],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<JournalPreparationOutput>, CommitPreflightError> {
    let mut outputs = budgeted_vec(publications.len(), "journal preparation outputs", budget)?;
    for publication in publications {
        let target = JournalPath::new_budgeted(&publication.relative_target, budget)
            .map_err(map_journal_preflight_error)?;
        let (expected, expected_digest) = match publication.expected {
            DestinationState::Existing(fingerprint) => (
                JournalExpectedDestination::Existing,
                Some(fingerprint.digest()),
            ),
            DestinationState::Absent => (JournalExpectedDestination::Absent, None),
            DestinationState::Directory
            | DestinationState::SymbolicLink
            | DestinationState::Other => {
                return Err(CommitPreflightError::Ownership(
                    "prepared destination expectation is unsupported".to_owned(),
                ));
            }
        };
        outputs.push(
            JournalPreparationOutput::new(
                publication.ordinal,
                &publication.logical_name,
                publication.source,
                target,
                expected,
                expected_digest,
                publication.expected_identity.clone(),
                publication.destination_parent_identity.clone(),
                publication.digest,
                publication.bytes,
                budget,
            )
            .map_err(map_journal_preflight_error)?,
        );
    }
    Ok(outputs)
}

fn journal_artifacts(
    publications: &[PreparedPublication],
    report: &CommitReport,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<JournalArtifact>, CommitPreflightError> {
    if publications.len() != report.artifacts().len() {
        return Err(CommitPreflightError::Ownership(
            "publication and report artifact counts disagree".to_owned(),
        ));
    }
    let mut artifacts = budgeted_vec(publications.len(), "journal artifact declarations", budget)?;
    for (publication, artifact) in publications.iter().zip(report.artifacts()) {
        let target = JournalPath::new_budgeted(&publication.relative_target, budget)
            .map_err(map_journal_preflight_error)?;
        let staging = ordinal_journal_path(
            "stage/",
            publication.ordinal,
            ".stage",
            "journal staging path",
            budget,
        )?;
        let backup = if matches!(publication.expected, DestinationState::Existing(_)) {
            Some(ordinal_journal_path(
                "backup/",
                publication.ordinal,
                ".backup",
                "journal backup path",
                budget,
            )?)
        } else {
            None
        };
        let old_digest = match publication.expected {
            DestinationState::Existing(fingerprint) => Some(fingerprint.digest()),
            DestinationState::Absent => None,
            DestinationState::Directory
            | DestinationState::SymbolicLink
            | DestinationState::Other => {
                return Err(CommitPreflightError::Ownership(
                    "prepared destination expectation is unsupported".to_owned(),
                ));
            }
        };
        let staged_identity = publication.staged_identity.clone().ok_or_else(|| {
            CommitPreflightError::Ownership("staged artifact has no captured identity".to_owned())
        })?;
        artifacts.push(
            JournalArtifact::new(
                artifact,
                target,
                publication.destination_parent_identity.clone(),
                staging,
                backup,
                old_digest,
                publication.expected_identity.clone(),
                staged_identity,
                budget,
            )
            .map_err(map_journal_preflight_error)?,
        );
    }
    Ok(artifacts)
}

fn ordinal_journal_path(
    prefix: &'static str,
    ordinal: u32,
    suffix: &'static str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<JournalPath, CommitPreflightError> {
    let requested = prefix
        .len()
        .checked_add(8)
        .and_then(|length| length.checked_add(suffix.len()))
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(usize_to_u64(requested, resource)?)?;
    let mut value = String::new();
    value
        .try_reserve_exact(requested)
        .map_err(|error| CommitPreflightError::Allocation {
            resource,
            requested,
            message: error.to_string(),
        })?;
    write!(&mut value, "{prefix}{ordinal:08}{suffix}").map_err(|_| {
        CommitPreflightError::Ownership("journal ordinal path formatting failed".to_owned())
    })?;
    if value.len() != requested {
        return Err(CommitPreflightError::Ownership(
            "journal ordinal path has a non-canonical length".to_owned(),
        ));
    }
    let actual = usize_to_u64(value.capacity(), resource)?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    JournalPath::from_owned(value).map_err(map_journal_preflight_error)
}

fn map_journal_preflight_error(error: JournalError) -> CommitPreflightError {
    match error {
        JournalError::Budget(error) => CommitPreflightError::Budget(error),
        JournalError::Allocation {
            resource,
            requested,
            message,
        } => CommitPreflightError::Allocation {
            resource,
            requested,
            message,
        },
        error => CommitPreflightError::Ownership(error.to_string()),
    }
}

fn commit_event_keys(
    publications: &[PreparedPublication],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<JournalEventKey>, CommitPreflightError> {
    let artifact_events = publications
        .iter()
        .try_fold(0_usize, |count, publication| {
            count.checked_add(if publication.expected_identity.is_some() {
                4
            } else {
                2
            })
        })
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "commit event plan",
        })?;
    let capacity = artifact_events
        .checked_add(6)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "commit event plan",
        })?;
    let mut keys = budgeted_vec(capacity, "commit event plan", budget)?;
    keys.push(JournalEventKey::StagingVerified);
    keys.push(JournalEventKey::Journaled);
    for publication in publications {
        if publication.expected_identity.is_some() {
            keys.push(JournalEventKey::BackupIntent(publication.ordinal));
            keys.push(JournalEventKey::BackupCaptured(publication.ordinal));
        }
        keys.push(JournalEventKey::PromotionIntent(publication.ordinal));
        keys.push(JournalEventKey::Promoted(publication.ordinal));
    }
    keys.push(JournalEventKey::Published);
    keys.push(JournalEventKey::BaselineInstalled);
    keys.push(JournalEventKey::Finalized);
    Ok(keys)
}

#[derive(Debug)]
enum PublishError {
    Message(String),
}

impl From<String> for PublishError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

fn publish_one(
    journal: &mut Journal,
    event_plan: &mut JournalEventPlan,
    publication: &PreparedPublication,
    execution: &mut PublicationExecution,
) -> Result<(), PublishError> {
    debug_assert_eq!(publication.ordinal, execution.ordinal);
    let stage = &execution.stage;
    let staged_identity = publication
        .staged_identity
        .as_ref()
        .ok_or_else(|| "staged artifact has no captured identity".to_owned())?;
    verify_journal_file(
        journal.stage_directory(),
        stage,
        publication.digest,
        Some(publication.bytes),
        Some(staged_identity),
    )
    .map_err(|error| error.to_string())?;
    if matches!(publication.expected, DestinationState::Existing(_)) {
        let backup = execution
            .backup
            .as_ref()
            .ok_or_else(|| "existing publication has no prepared backup path".to_owned())?;
        journal
            .append_planned(
                event_plan,
                JournalEventKey::BackupIntent(publication.ordinal),
            )
            .map_err(|error| error.to_string())?;
        #[cfg(test)]
        test_crash_artifact_failpoint("backup_intent", publication.ordinal);
        let expected_identity = publication
            .expected_identity
            .as_ref()
            .ok_or_else(|| "existing publication target has no captured identity".to_owned())?;
        let DestinationState::Existing(expected) = publication.expected else {
            unreachable!();
        };
        capture_external_regular_in_journal_directory(
            &publication.target,
            journal.backup_directory(),
            backup,
            expected_identity,
            Some(expected.digest()),
            &publication.destination_parent_identity,
        )
        .map_err(|error| error.to_string())?;
        #[cfg(test)]
        test_crash_artifact_failpoint("backup_renamed", publication.ordinal);
        verify_journal_file(
            journal.backup_directory(),
            backup,
            expected.digest(),
            None,
            publication.expected_identity.as_ref(),
        )
        .map_err(|error| error.to_string())?;
        copy_security_metadata_between_journal_directories(
            journal.backup_directory(),
            backup,
            journal.stage_directory(),
            stage,
            expected_identity,
            staged_identity,
            execution
                .security_metadata
                .as_mut()
                .ok_or_else(|| {
                    "existing publication has no reserved security metadata budget".to_owned()
                })?
                .budget_mut(),
        )
        .map_err(|error| error.to_string())?;
        journal
            .append_planned(
                event_plan,
                JournalEventKey::BackupCaptured(publication.ordinal),
            )
            .map_err(|error| error.to_string())?;
    }
    // This is the publication linearization check. It immediately precedes the durable intent so
    // already-visible stage corruption cannot mutate the target first.
    verify_journal_file(
        journal.stage_directory(),
        stage,
        publication.digest,
        Some(publication.bytes),
        Some(staged_identity),
    )
    .map_err(|error| error.to_string())?;
    journal
        .append_planned(
            event_plan,
            JournalEventKey::PromotionIntent(publication.ordinal),
        )
        .map_err(|error| error.to_string())?;
    #[cfg(test)]
    test_crash_artifact_failpoint("promotion_intent", publication.ordinal);
    promote_journal_regular_to_external(
        journal.stage_directory(),
        stage,
        &publication.target,
        staged_identity,
        Some(publication.digest),
        &publication.destination_parent_identity,
    )
    .map_err(|error| error.to_string())?;
    #[cfg(test)]
    test_crash_artifact_failpoint("promotion_renamed", publication.ordinal);
    verify_file(
        &publication.target,
        publication.digest,
        Some(publication.bytes),
        Some(staged_identity),
        &publication.destination_parent_identity,
    )
    .map_err(|error| error.to_string())?;
    journal
        .append_planned(event_plan, JournalEventKey::Promoted(publication.ordinal))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn verify_file(
    path: &Path,
    expected: DigestV1,
    expected_len: Option<u64>,
    expected_identity: Option<&FileIdentity>,
    expected_parent: &DirectoryIdentity,
) -> io::Result<()> {
    let mut file = open_readonly_regular_in_parent(path, expected_parent)?;
    let identity = opened_file_identity(&file)?;
    let length = file.metadata()?.len();
    if expected_len.is_some_and(|expected_len| expected_len != length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published artifact length changed",
        ));
    }
    if expected_identity.is_some_and(|expected_identity| expected_identity != &identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published artifact identity changed",
        ));
    }
    let actual = DigestV1::hash_reader(&mut file, length)?;
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published artifact digest changed",
        ));
    }
    Ok(())
}

fn verify_journal_file(
    directory: &JournalDirectory,
    path: &Path,
    expected: DigestV1,
    expected_len: Option<u64>,
    expected_identity: Option<&FileIdentity>,
) -> io::Result<()> {
    let mut file = open_journal_regular_in_directory(directory, path)?;
    let identity = opened_file_identity(&file)?;
    let length = file.metadata()?.len();
    if expected_len.is_some_and(|expected_len| expected_len != length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published artifact length changed",
        ));
    }
    if expected_identity.is_some_and(|expected_identity| expected_identity != &identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published artifact identity changed",
        ));
    }
    let actual = DigestV1::hash_reader(&mut file, length)?;
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published artifact digest changed",
        ));
    }
    Ok(())
}

fn prepared_revision_changed(workspace: &AssetWorkspace, baseline: &PreparedBaseline) -> bool {
    workspace.workspace_id() != baseline.expected.workspace()
        || workspace.revision() != baseline.expected.revision()
        || baseline.next.workspace() != baseline.expected.workspace()
        || baseline.next.revision() == baseline.expected.revision()
}

#[derive(Debug, Error)]
enum PreparePublicationError {
    #[error(transparent)]
    Budget(BudgetError),
    #[error("prepared revision changed from {expected} to {actual}")]
    StaleRevision {
        expected: WorkspaceRevision,
        actual: WorkspaceRevision,
    },
    #[error("source {source_id:?} changed from {expected} to {actual}")]
    SourceConflict {
        source_id: SourceId,
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
    #[error("destination output {output} changed from {expected:?} to {actual:?}")]
    DestinationConflict {
        output: usize,
        expected: DestinationState,
        actual: DestinationState,
    },
    #[error("publication is blocked: {0}")]
    PublishBlocked(String),
    #[error("publication may be retried: {0}")]
    Retryable(String),
    #[error("publication requires recovery at {locator:?}: {message}")]
    RecoveryRequired {
        locator: RecoveryLocator,
        message: String,
    },
}

fn validate_publication_hardlinks(
    publications: &[PreparedPublication],
) -> Result<(), PreparePublicationError> {
    for publication in publications {
        if publication.expected_identity.is_some() {
            ensure_single_hardlink(&publication.target).map_err(|error| {
                PreparePublicationError::PublishBlocked(format!(
                    "publication target {:?} must have exactly one hard link: {error}",
                    publication.target
                ))
            })?;
        }
    }
    Ok(())
}

fn map_source_proof_error(error: PhysicalDependencyProofError) -> PreparePublicationError {
    match error {
        PhysicalDependencyProofError::Budget(error) => PreparePublicationError::Budget(error),
        PhysicalDependencyProofError::ArithmeticOverflow { resource } => {
            PreparePublicationError::Budget(BudgetError::ArithmeticOverflow { resource })
        }
        PhysicalDependencyProofError::ContentChanged {
            source_id,
            expected,
            actual,
        } => PreparePublicationError::SourceConflict {
            source_id,
            expected,
            actual,
        },
        PhysicalDependencyProofError::Catalog {
            source_id,
            expected: Some(expected),
            source,
        } => match *source {
            CatalogError::VerifiedFingerprintMismatch { actual, .. } => {
                PreparePublicationError::SourceConflict {
                    source_id,
                    expected,
                    actual,
                }
            }
            error => map_catalog_prepare_error(error),
        },
        PhysicalDependencyProofError::Catalog { source, .. } => map_catalog_prepare_error(*source),
        error @ PhysicalDependencyProofError::Allocation { .. } => {
            PreparePublicationError::Retryable(error.to_string())
        }
    }
}

fn map_destination_proof_error(error: DestinationProofError) -> PreparePublicationError {
    match error {
        DestinationProofError::Budget(error) => PreparePublicationError::Budget(error),
        DestinationProofError::ArithmeticOverflow { resource } => {
            PreparePublicationError::Budget(BudgetError::ArithmeticOverflow { resource })
        }
        DestinationProofError::ObservationMismatch {
            output,
            expected,
            actual,
        } => PreparePublicationError::DestinationConflict {
            output,
            expected,
            actual,
        },
        DestinationProofError::FileIdentityChanged {
            output,
            expected_fingerprint,
        } => PreparePublicationError::DestinationConflict {
            output,
            expected: DestinationState::Existing(expected_fingerprint),
            actual: DestinationState::Other,
        },
        DestinationProofError::ParentIdentityChanged { output }
        | DestinationProofError::PathComponentChanged { output, .. } => {
            PreparePublicationError::DestinationConflict {
                output,
                expected: DestinationState::Absent,
                actual: DestinationState::Other,
            }
        }
        DestinationProofError::Catalog { source, .. } => map_catalog_prepare_error(*source),
        error @ DestinationProofError::Allocation { .. } => {
            PreparePublicationError::Retryable(error.to_string())
        }
        DestinationProofError::Io { kind, message, .. } => map_prejournal_io(
            "validate publication destination",
            io::Error::new(kind, message),
        ),
        error => PreparePublicationError::PublishBlocked(error.to_string()),
    }
}

fn map_catalog_prepare_error(error: CatalogError) -> PreparePublicationError {
    match error {
        CatalogError::Budget(error) => PreparePublicationError::Budget(error),
        CatalogError::AllocationSizeOverflow { resource } => {
            PreparePublicationError::Budget(BudgetError::ArithmeticOverflow { resource })
        }
        error @ CatalogError::AllocationFailed { .. } => {
            PreparePublicationError::Retryable(error.to_string())
        }
        CatalogError::VerifiedPhysicalBindingIo {
            kind: io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut,
            message,
            ..
        } => PreparePublicationError::Retryable(message),
        error => PreparePublicationError::PublishBlocked(error.to_string()),
    }
}

fn map_preflight_error(error: CommitPreflightError) -> PreparePublicationError {
    match error {
        CommitPreflightError::Budget(error) => PreparePublicationError::Budget(error),
        error @ CommitPreflightError::Allocation { .. } => {
            PreparePublicationError::Retryable(error.to_string())
        }
        error => PreparePublicationError::PublishBlocked(error.to_string()),
    }
}

fn map_baseline_prepare_error(error: baseline::BaselineBuildError) -> PreparePublicationError {
    match error.into_budget() {
        Ok(error) => PreparePublicationError::Budget(error),
        Err(error) if error.is_retryable_prejournal() => {
            PreparePublicationError::Retryable(error.to_string())
        }
        Err(error) => PreparePublicationError::PublishBlocked(error.to_string()),
    }
}

fn map_recovery_baseline_prepare_error(
    error: RecoveryBaselineWriteError,
) -> PreparePublicationError {
    match error {
        RecoveryBaselineWriteError::Budget(error) => PreparePublicationError::Budget(error),
        RecoveryBaselineWriteError::Io(error) => {
            map_prejournal_io("write recovery baseline", error)
        }
        error @ RecoveryBaselineWriteError::Allocation(_) => {
            PreparePublicationError::Retryable(error.to_string())
        }
        error @ RecoveryBaselineWriteError::Invariant(_) => {
            PreparePublicationError::PublishBlocked(error.to_string())
        }
    }
}

fn map_security_metadata_prepare_error(error: SecurityMetadataError) -> PreparePublicationError {
    match error {
        SecurityMetadataError::Budget(error) => PreparePublicationError::Budget(error),
        SecurityMetadataError::Io(error) => map_prejournal_io("copy security metadata", error),
    }
}

fn map_journal_layout_prepare_error(error: JournalError) -> PreparePublicationError {
    match error {
        JournalError::Budget(error) => PreparePublicationError::Budget(error),
        error @ JournalError::Allocation { .. } => {
            PreparePublicationError::Retryable(error.to_string())
        }
        error => PreparePublicationError::PublishBlocked(error.to_string()),
    }
}

fn map_prejournal_io(operation: &'static str, error: io::Error) -> PreparePublicationError {
    let message = format!("{operation}: {error}");
    match error.kind() {
        io::ErrorKind::AlreadyExists
        | io::ErrorKind::CrossesDevices
        | io::ErrorKind::InvalidInput
        | io::ErrorKind::PermissionDenied
        | io::ErrorKind::Unsupported => PreparePublicationError::PublishBlocked(message),
        _ => PreparePublicationError::Retryable(message),
    }
}

fn map_unpublished_journal_prepare_error(error: JournalError) -> PreparePublicationError {
    match error {
        JournalError::Budget(source) => PreparePublicationError::Budget(source),
        JournalError::Io(error) => map_prejournal_io("create publication journal", error),
        source @ JournalError::Allocation { .. } => {
            PreparePublicationError::Retryable(source.to_string())
        }
        source @ (JournalError::Json(_)
        | JournalError::NestingDepthExceeded { .. }
        | JournalError::UnsupportedVersion(_)
        | JournalError::InvalidPath { .. }
        | JournalError::InvalidManifest(_)
        | JournalError::InvalidEvent(_)
        | JournalError::DocumentTooLarge { .. }
        | JournalError::TooManyEvents(_)
        | JournalError::SequenceMismatch { .. }
        | JournalError::PreviousDigestMismatch { .. }
        | JournalError::DigestMismatch { .. }
        | JournalError::TransactionMismatch { .. }) => {
            PreparePublicationError::PublishBlocked(source.to_string())
        }
    }
}

/// Commit failure classification.
///
/// Only resource exhaustion and transient pre-journal failures return the exact prepared change.
/// Semantic mismatches, conflicts, unsupported guarantees, and every post-journal outcome are
/// terminal and consume it.
#[derive(Debug, Error)]
pub enum CommitError {
    #[error("prepared change belongs to workspace {actual}, not {expected}")]
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("prepared change is stale: expected revision {expected}, current revision {actual}")]
    StaleRevision {
        expected: WorkspaceRevision,
        actual: WorkspaceRevision,
    },
    #[error("prepared change has no semantic effect")]
    NoEffect,
    #[error("publication preflight exceeded its caller-owned budget: {source}")]
    Budget {
        #[source]
        source: BudgetError,
        prepared: Box<PreparedChange>,
    },
    #[error("source {source_id:?} changed from {expected} to {actual} after prepare")]
    SourceConflict {
        source_id: SourceId,
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
    #[error("destination output {output} changed from {expected:?} to {actual:?} after prepare")]
    DestinationConflict {
        output: usize,
        expected: CommitDestinationState,
        actual: CommitDestinationState,
    },
    #[error("publication is blocked by an unavailable platform guarantee: {message}")]
    PublishBlocked { message: String },
    #[error("publication preflight can be retried: {message}")]
    Retryable {
        message: String,
        prepared: Box<PreparedChange>,
    },
    #[error("publication requires recovery at {locator:?}: {message}")]
    RecoveryRequired {
        locator: RecoveryLocator,
        message: String,
    },
    #[error("publication contract is invalid: {0}")]
    Contract(#[from] CommitContractError),
}

impl CommitError {
    #[must_use]
    pub fn prepared(&self) -> Option<&PreparedChange> {
        match self {
            Self::Budget { prepared, .. } | Self::Retryable { prepared, .. } => Some(prepared),
            Self::WorkspaceMismatch { .. }
            | Self::StaleRevision { .. }
            | Self::NoEffect
            | Self::SourceConflict { .. }
            | Self::DestinationConflict { .. }
            | Self::PublishBlocked { .. }
            | Self::RecoveryRequired { .. }
            | Self::Contract(_) => None,
        }
    }

    pub fn into_prepared(self) -> Option<PreparedChange> {
        match self {
            Self::Budget { prepared, .. } | Self::Retryable { prepared, .. } => Some(*prepared),
            Self::WorkspaceMismatch { .. }
            | Self::StaleRevision { .. }
            | Self::NoEffect
            | Self::SourceConflict { .. }
            | Self::DestinationConflict { .. }
            | Self::PublishBlocked { .. }
            | Self::RecoveryRequired { .. }
            | Self::Contract(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommitContractError {
    #[error("commit report version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("commit report transaction and change set disagree")]
    TransactionMismatch,
    #[error("commit report workspace and change set disagree")]
    WorkspaceMismatch,
    #[error("commit report revisions and change set disagree")]
    RevisionMismatch,
    #[error("commit report recovery locator belongs to another transaction")]
    RecoveryTransactionMismatch,
    #[error("commit report contains no artifacts")]
    EmptyArtifactSet,
    #[error("commit report artifacts are not in strict logical-name order")]
    ArtifactOrder,
}

impl fmt::Display for PublicationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "recoverable in-place publication under {:?}",
            self.root
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{AssetLoadLimits, SourceKind, WorkspaceId};

    #[test]
    fn publication_target_is_explicitly_in_place() {
        let directory = tempfile::tempdir().unwrap();
        let target = PublicationTarget::in_place(directory.path()).unwrap();
        assert_eq!(target.root(), directory.path().canonicalize().unwrap());
        assert!(target.to_string().contains("in-place"));
    }

    #[test]
    fn publication_target_rebuilds_a_deterministic_recovery_locator() {
        let directory = tempfile::tempdir().unwrap();
        let target = PublicationTarget::in_place(directory.path()).unwrap();
        let transaction = TransactionId::new(DigestV1::hash_bytes(b"recovery locator"));

        let locator = target.recovery_locator(transaction);

        assert_eq!(locator.transaction(), transaction);
        assert_eq!(
            locator.root(),
            JournalLayout::new(target.root(), transaction, target.identity().clone()).directory()
        );
    }

    #[test]
    fn recovery_layout_publishes_preparation_before_the_transaction_directory() {
        let directory = tempfile::tempdir().unwrap();
        let transaction = TransactionId::new(DigestV1::hash_bytes(b"preparation locator"));
        let root_identity = observe_directory_identity(directory.path()).unwrap();
        let layout = JournalLayout::new(directory.path(), transaction, root_identity);

        assert_eq!(
            layout.preparation_path().parent(),
            layout.directory().parent()
        );
        assert_ne!(layout.preparation_path(), layout.manifest_path());
        assert!(
            layout
                .preparation_path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".prepare.v2.json"))
        );
    }

    #[test]
    fn artifact_report_preserves_exact_identity() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let source = SourceId::new(workspace, SourceKind::Yaml, 1).unwrap();
        let digest = DigestV1::hash_bytes(b"artifact");
        let report = CommitArtifactReport::new("root-00000000".to_owned(), source, digest, 8);
        assert_eq!(report.logical_name(), "root-00000000");
        assert_eq!(report.source(), source);
        assert_eq!(report.digest(), digest);
        assert_eq!(report.bytes(), 8);
    }

    #[test]
    fn prejournal_platform_guarantee_failures_are_not_retryable_io() {
        for kind in [
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::CrossesDevices,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Unsupported,
        ] {
            assert!(matches!(
                map_prejournal_io("test operation", io::Error::from(kind)),
                PreparePublicationError::PublishBlocked(_)
            ));
        }
        assert!(matches!(
            map_prejournal_io("test operation", io::Error::other("transient")),
            PreparePublicationError::Retryable(_)
        ));
    }

    #[test]
    fn semantic_prejournal_failures_are_terminal() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let source = SourceId::new(workspace, SourceKind::Yaml, 1).unwrap();
        assert!(matches!(
            map_catalog_prepare_error(CatalogError::UnknownSource(source)),
            PreparePublicationError::PublishBlocked(_)
        ));
        assert!(matches!(
            map_preflight_error(CommitPreflightError::Ownership(
                "invalid ownership".to_owned()
            )),
            PreparePublicationError::PublishBlocked(_)
        ));
        assert!(matches!(
            map_baseline_prepare_error(baseline::BaselineBuildError::Parse {
                message: "invalid prepared image".to_owned(),
            }),
            PreparePublicationError::PublishBlocked(_)
        ));
        assert!(matches!(
            map_recovery_baseline_prepare_error(RecoveryBaselineWriteError::Invariant(
                "invalid checkpoint".to_owned(),
            )),
            PreparePublicationError::PublishBlocked(_)
        ));
    }

    #[test]
    fn cleanup_prejournal_error_uses_the_callers_remaining_budget() {
        let directory = tempfile::tempdir().unwrap();
        let transaction = TransactionId::new(DigestV1::hash_bytes(b"cleanup budget"));
        let root_identity = observe_directory_identity(directory.path()).unwrap();
        let layout = JournalLayout::new(directory.path(), transaction, root_identity);
        let preparation = layout.preparation_path();
        std::fs::create_dir_all(preparation.parent().expect("preparation parent")).unwrap();
        std::fs::write(preparation, b"{}").unwrap();
        let root = open_commit_root(layout.parent(), layout.root_identity()).unwrap();
        let namespace = open_journal_namespace(&root).unwrap();
        let access = journal_access(&root, &namespace);

        let limits = AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let error = cleanup_prejournal_error(
            &layout,
            &access,
            PreparePublicationError::PublishBlocked("synthetic prejournal failure".to_owned()),
            "synthetic prejournal cleanup",
            &mut budget,
        );

        let PreparePublicationError::RecoveryRequired { locator, message } = error else {
            panic!("budget-exhausted cleanup must preserve a recoverable transaction");
        };
        assert_eq!(locator.root(), layout.directory());
        assert!(message.contains("asset load budget exceeded for bytes"));
        assert!(
            preparation.is_file(),
            "cleanup must retain preparation evidence"
        );
    }
}
