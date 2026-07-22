//! Deterministic recovery for durable publication journals.
//!
//! Recovery deliberately separates observation from mutation. The pure state
//! machine below decides one sticky direction from journal facts, filesystem
//! evidence, and the currently installed workspace baseline. Only after that
//! decision is durably appended may the executor move an artifact.

use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, WorkspaceId, WorkspaceRevision, vec_allocation_bytes,
};

use super::super::portable_path::{PortablePathError, slash_key};

use super::journal::{
    Journal, JournalArtifact, JournalError, JournalEvent, JournalEventKey, JournalEventKind,
    JournalEventPlan, JournalLayout, JournalPath, RecoveryDirection, matches_ordinal_journal_path,
};
use super::platform::{
    CommitGuard, DirectoryIdentity, FileIdentity, capture_existing_in_parent,
    capture_matching_digest_in_parent, copy_security_metadata, observe_directory_identity,
    open_readonly_regular_in_parent, opened_file_identity,
};
#[cfg(test)]
use super::platform::{capture_existing, observe_file_identity};
use super::{AssetWorkspace, CommitReport, RecoveryLocator};

/// Direction selected by deterministic journal recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDisposition {
    /// Finish publishing the exact prepared artifact set.
    Forward,
    /// Restore the complete pre-publication artifact set.
    Rollback,
    /// Preserve all evidence because neither direction is provably safe.
    Blocked,
}

/// Terminal result of recovering one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// The canonical commit result was finalized and may be redelivered.
    Committed(Box<CommitReport>),
    /// The pre-publication artifact set was restored.
    RolledBack(RecoveryLocator),
}

impl RecoveryOutcome {
    #[must_use]
    pub const fn committed(&self) -> Option<&CommitReport> {
        match self {
            Self::Committed(report) => Some(report),
            Self::RolledBack(_) => None,
        }
    }

    #[must_use]
    pub const fn recovery(&self) -> &RecoveryLocator {
        match self {
            Self::Committed(report) => report.recovery(),
            Self::RolledBack(locator) => locator,
        }
    }
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
    #[error(
        "published revision {committed} cannot be installed from this journal over current revision {actual}"
    )]
    BaselineUnavailable {
        committed: WorkspaceRevision,
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
        locator: RecoveryLocator,
        message: String,
    },
    #[error("recovery is blocked: {reason}")]
    Blocked {
        locator: RecoveryLocator,
        reason: Box<RecoveryBlockedReason>,
    },
    #[error("recovery exceeded its caller-owned budget: {source}")]
    Budget {
        locator: RecoveryLocator,
        #[source]
        source: BudgetError,
    },
}

impl RecoveryError {
    #[must_use]
    pub const fn locator(&self) -> &RecoveryLocator {
        match self {
            Self::Busy { locator, .. }
            | Self::Blocked { locator, .. }
            | Self::Budget { locator, .. } => locator,
        }
    }

    #[must_use]
    pub const fn blocked_reason(&self) -> Option<&RecoveryBlockedReason> {
        match self {
            Self::Blocked { reason, .. } => Some(reason),
            Self::Busy { .. } | Self::Budget { .. } => None,
        }
    }
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
        if self.had_original {
            self.target == EntryEvidence::Old && self.backup == EntryEvidence::Missing
        } else {
            self.target == EntryEvidence::Missing
                && self.backup == EntryEvidence::Missing
                && self.staging != EntryEvidence::Unexpected
        }
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
    stage_parent_identity: DirectoryIdentity,
    backup_parent_identity: DirectoryIdentity,
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
        return match observation.baseline {
            BaselineObservation::Base | BaselineObservation::Committed => RecoveryPlan::forward(),
            BaselineObservation::Other => {
                RecoveryPlan::blocked(RecoveryBlockedReason::InvalidEventSequence {
                    message:
                        "published bytes do not match the base or committed workspace revision"
                            .to_owned(),
                })
            }
        };
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
    if observation.baseline != BaselineObservation::Base {
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
    if !observation.events.baseline_installed {
        keys.push(JournalEventKey::BaselineInstalled);
    }
    if !observation.events.finalized {
        keys.push(JournalEventKey::Finalized);
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
    /// Recovers exactly one durable transaction named by a previous commit result.
    ///
    /// The locator is validated against its deterministic destination-parent
    /// namespace before any journal path is opened. A published transaction can
    /// only be redelivered when this workspace already owns its committed
    /// baseline; the current journal format intentionally does not pretend it can
    /// reconstruct missing parse/catalog state after a process restart.
    pub fn recover_at(
        &mut self,
        locator: &RecoveryLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        self.recover_with_intent(locator, budget, RecoveryIntent::Resume)
    }

    /// Explicitly rolls back one unfinished transaction when its durable
    /// evidence still proves a safe rollback.
    ///
    /// Published, finalized, or sticky-forward transactions are rejected. An
    /// explicit abandon never deletes journal evidence directly.
    pub fn abandon_at(
        &mut self,
        locator: &RecoveryLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        self.recover_with_intent(locator, budget, RecoveryIntent::Abandon)
    }

    fn recover_with_intent(
        &mut self,
        locator: &RecoveryLocator,
        budget: &mut AssetLoadBudget,
        intent: RecoveryIntent,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        let layout = layout_from_locator(locator).map_err(|reason| RecoveryError::Blocked {
            locator: locator.clone(),
            reason: Box::new(reason),
        })?;
        let _guard =
            CommitGuard::acquire(layout.parent()).map_err(|error| RecoveryError::Busy {
                locator: locator.clone(),
                message: error.to_string(),
            })?;
        let mut journal = Journal::open(layout, budget)
            .map_err(|error| map_journal_open_error(locator, error))?;
        recover_open_journal(self, &mut journal, locator, intent, budget)
    }
}

fn recover_open_journal(
    workspace: &mut AssetWorkspace,
    journal: &mut Journal,
    locator: &RecoveryLocator,
    intent: RecoveryIntent,
    budget: &mut AssetLoadBudget,
) -> Result<RecoveryOutcome, RecoveryError> {
    let report = journal
        .manifest()
        .report(locator.root(), budget)
        .map_err(|error| map_journal_mutation_error(locator, error))?;
    if report.workspace_id() != workspace.workspace_id() {
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
    let baseline = if workspace.revision() == report.committed_revision() {
        BaselineObservation::Committed
    } else if workspace.revision() == report.base_revision() {
        BaselineObservation::Base
    } else {
        BaselineObservation::Other
    };
    if baseline == BaselineObservation::Other {
        return Err(blocked(
            locator,
            RecoveryBlockedReason::BaselineUnavailable {
                committed: report.committed_revision(),
                actual: workspace.revision(),
            },
        ));
    }
    let (execution, artifacts) = observe_execution(journal, budget)
        .map_err(|error| map_observation_error(locator, error))?;
    let mut observation = RecoveryObservation {
        events,
        artifacts,
        baseline,
    };
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
                && workspace.revision() != report.committed_revision()
            {
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::BaselineUnavailable {
                        committed: report.committed_revision(),
                        actual: workspace.revision(),
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
                execute_owned_corruption_repairs(&execution, &repairs)
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
                return Ok(RecoveryOutcome::Committed(Box::new(report)));
            }
            let already_published = observation
                .artifacts
                .iter()
                .all(|artifact| artifact.is_published());
            let prebuilt_baseline = if observation.baseline == BaselineObservation::Base {
                Some(prebuild_recovery_baseline(
                    workspace,
                    journal,
                    &observation.artifacts,
                    locator,
                    budget,
                )?)
            } else {
                None
            };
            let event_keys = recovery_event_keys(&observation, plan.disposition, budget)
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
                )
                .map_err(|error| map_execution_error(locator, error))?;
            }

            if workspace.revision() == report.base_revision() {
                let baseline = prebuilt_baseline.expect("base recovery prebuilds its baseline");
                if !workspace.install_state_if_current(&baseline.expected, baseline.next) {
                    return Err(blocked(
                        locator,
                        RecoveryBlockedReason::BaselineUnavailable {
                            committed: report.committed_revision(),
                            actual: workspace.revision(),
                        },
                    ));
                }
                observation.baseline = BaselineObservation::Committed;
            } else if workspace.revision() != report.committed_revision() {
                return Err(blocked(
                    locator,
                    RecoveryBlockedReason::BaselineUnavailable {
                        committed: report.committed_revision(),
                        actual: workspace.revision(),
                    },
                ));
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
            event_plan
                .finish()
                .map_err(|error| map_journal_mutation_error(locator, error))?;
            Ok(RecoveryOutcome::Committed(Box::new(report)))
        }
        RecoveryDisposition::Rollback => {
            if observation.events.finalized && observation.events.abandoned {
                return Ok(RecoveryOutcome::RolledBack(locator.clone()));
            }
            let event_keys = recovery_event_keys(&observation, plan.disposition, budget)
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
            roll_back(&mut observation.artifacts, &execution)
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
            Ok(RecoveryOutcome::RolledBack(locator.clone()))
        }
    }
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

fn layout_from_locator(locator: &RecoveryLocator) -> Result<JournalLayout, RecoveryBlockedReason> {
    let directory = locator.root();
    if !directory.is_absolute()
        || directory
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(invalid_locator(
            "the transaction path is not absolute and normalized",
        ));
    }
    let version = directory
        .parent()
        .ok_or_else(|| invalid_locator("the version directory is missing"))?;
    let recovery = version
        .parent()
        .ok_or_else(|| invalid_locator("the recovery directory is missing"))?;
    let parent = recovery
        .parent()
        .ok_or_else(|| invalid_locator("the destination parent is missing"))?;
    if version.file_name().and_then(|name| name.to_str()) != Some("v1")
        || recovery.file_name().and_then(|name| name.to_str()) != Some(".unity-asset-recovery")
    {
        return Err(invalid_locator("the recovery namespace is not version 1"));
    }
    let layout = JournalLayout::new(parent, locator.transaction());
    if layout.directory() != directory {
        return Err(invalid_locator(
            "the transaction directory does not match its transaction digest",
        ));
    }
    validate_directory(parent, "destination parent")?;
    validate_directory(recovery, "recovery directory")?;
    validate_directory(version, "recovery version directory")?;
    validate_directory(directory, "transaction directory")?;
    Ok(layout)
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
        Ok(Self {
            stage_parent_identity: manifest.directories().stage().clone(),
            backup_parent_identity: manifest.directories().backup().clone(),
            artifacts,
        })
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
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return Err(RecoveryBlockedReason::UnsafePath {
                artifact: artifact.to_owned(),
                role,
            });
        };
        current.push(component);
        if components.peek().is_none() {
            break;
        }
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| RecoveryBlockedReason::Io {
                message: format!("failed to inspect an {role} ancestor: {error}"),
            })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(RecoveryBlockedReason::UnsafePath {
                artifact: artifact.to_owned(),
                role,
            });
        }
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
    let requested = root
        .as_os_str()
        .len()
        .checked_add(relative.as_str().len())
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
    path.push(relative.as_str());
    let actual =
        u64::try_from(path.capacity()).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(path)
}

fn execute_owned_corruption_repairs(
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
                    capture_existing_in_parent(
                        &paths.target,
                        &paths.staging,
                        &paths.new_identity,
                        &paths.target_parent_identity,
                        &execution.stage_parent_identity,
                    )?;
                }
                capture_existing_in_parent(
                    backup,
                    &paths.target,
                    old_identity,
                    &execution.backup_parent_identity,
                    &paths.target_parent_identity,
                )?;
            }
            OwnedCorruptionRepair::RestoreAbsence { ordinal } => {
                let paths = execution.artifacts.get(*ordinal).ok_or_else(|| {
                    ExecutionError::Blocked(invalid_event(
                        "owned-corruption repair ordinal is outside its execution plan",
                    ))
                })?;
                capture_existing_in_parent(
                    &paths.target,
                    &paths.staging,
                    &paths.new_identity,
                    &paths.target_parent_identity,
                    &execution.stage_parent_identity,
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
        forward_artifact(journal, facts, observations, execution, index, event_plan)?;
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
                capture_matching_digest_in_parent(
                    &paths.target,
                    backup,
                    old_identity,
                    old,
                    &paths.target_parent_identity,
                    &execution.backup_parent_identity,
                )?;
                verify_digest_precharged(
                    backup,
                    old,
                    old_identity,
                    &execution.backup_parent_identity,
                )?;
                observations[index].target = EntryEvidence::Missing;
                observations[index].backup = EntryEvidence::Old;
                copy_security_metadata(
                    backup,
                    &paths.staging,
                    old_identity,
                    &execution.backup_parent_identity,
                    &paths.new_identity,
                    &execution.stage_parent_identity,
                )?;
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
                verify_digest_precharged(
                    backup,
                    old,
                    old_identity,
                    &execution.backup_parent_identity,
                )?;
                copy_security_metadata(
                    backup,
                    &paths.staging,
                    old_identity,
                    &execution.backup_parent_identity,
                    &paths.new_identity,
                    &execution.stage_parent_identity,
                )?;
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
    verify_digest_precharged(
        &paths.staging,
        paths.new_digest,
        &paths.new_identity,
        &execution.stage_parent_identity,
    )?;
    append_artifact_once(
        journal,
        &mut facts.promotion_intent,
        paths.ordinal,
        ArtifactEvent::PromotionIntent,
        event_plan,
    )?;
    capture_matching_digest_in_parent(
        &paths.staging,
        &paths.target,
        &paths.new_identity,
        paths.new_digest,
        &execution.stage_parent_identity,
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

fn roll_back(
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
                    capture_matching_digest_in_parent(
                        backup,
                        &paths.target,
                        old_identity,
                        old,
                        &execution.backup_parent_identity,
                        &paths.target_parent_identity,
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
                    if observation.target == EntryEvidence::New {
                        capture_matching_digest_in_parent(
                            &paths.target,
                            &paths.staging,
                            &paths.new_identity,
                            paths.new_digest,
                            &paths.target_parent_identity,
                            &execution.stage_parent_identity,
                        )?;
                    } else {
                        capture_existing_in_parent(
                            &paths.target,
                            &paths.staging,
                            &paths.new_identity,
                            &paths.target_parent_identity,
                            &execution.stage_parent_identity,
                        )?;
                    }
                    capture_matching_digest_in_parent(
                        backup,
                        &paths.target,
                        old_identity,
                        old,
                        &execution.backup_parent_identity,
                        &paths.target_parent_identity,
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
                    capture_existing_in_parent(
                        backup,
                        &paths.target,
                        old_identity,
                        &execution.backup_parent_identity,
                        &paths.target_parent_identity,
                    )?;
                    observations[index].target = EntryEvidence::CorruptOld;
                    observations[index].backup = EntryEvidence::Missing;
                    return Err(unexpected_execution_artifact(paths.ordinal));
                }
                (
                    EntryEvidence::New | EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::CorruptOld,
                ) => {
                    if observation.target == EntryEvidence::New {
                        capture_matching_digest_in_parent(
                            &paths.target,
                            &paths.staging,
                            &paths.new_identity,
                            paths.new_digest,
                            &paths.target_parent_identity,
                            &execution.stage_parent_identity,
                        )?;
                    } else {
                        capture_existing_in_parent(
                            &paths.target,
                            &paths.staging,
                            &paths.new_identity,
                            &paths.target_parent_identity,
                            &execution.stage_parent_identity,
                        )?;
                    }
                    capture_existing_in_parent(
                        backup,
                        &paths.target,
                        old_identity,
                        &execution.backup_parent_identity,
                        &paths.target_parent_identity,
                    )?;
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
                    if observation.target == EntryEvidence::New {
                        capture_matching_digest_in_parent(
                            &paths.target,
                            &paths.staging,
                            &paths.new_identity,
                            paths.new_digest,
                            &paths.target_parent_identity,
                            &execution.stage_parent_identity,
                        )?;
                    } else {
                        capture_existing_in_parent(
                            &paths.target,
                            &paths.staging,
                            &paths.new_identity,
                            &paths.target_parent_identity,
                            &execution.stage_parent_identity,
                        )?;
                    }
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

fn unexpected_verification() -> ExecutionError {
    ExecutionError::Blocked(RecoveryBlockedReason::UnexpectedEvidence {
        artifact: "post-move-verification".to_owned(),
    })
}

#[derive(Debug, Error)]
enum ExecutionError {
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
        locator: locator.clone(),
        reason: Box::new(reason),
    }
}

fn invalid_journal(message: String) -> RecoveryBlockedReason {
    RecoveryBlockedReason::InvalidJournal { message }
}

fn map_journal_open_error(locator: &RecoveryLocator, error: JournalError) -> RecoveryError {
    match error {
        JournalError::Budget(source) => RecoveryError::Budget {
            locator: locator.clone(),
            source,
        },
        error => blocked(locator, invalid_journal(error.to_string())),
    }
}

fn map_journal_mutation_error(locator: &RecoveryLocator, error: JournalError) -> RecoveryError {
    match error {
        JournalError::Budget(source) => RecoveryError::Budget {
            locator: locator.clone(),
            source,
        },
        error => blocked(locator, invalid_journal(error.to_string())),
    }
}

fn map_baseline_error(
    locator: &RecoveryLocator,
    error: super::baseline::BaselineBuildError,
) -> RecoveryError {
    match error.into_budget() {
        Ok(source) => RecoveryError::Budget {
            locator: locator.clone(),
            source,
        },
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
            locator: locator.clone(),
            source,
        },
        ObservationError::Blocked(reason) => blocked(locator, reason),
    }
}

fn map_execution_error(locator: &RecoveryLocator, error: ExecutionError) -> RecoveryError {
    match error {
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

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
    const YAML: &[u8] =
        b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";
    const RESOURCE_YAML: &[u8] = b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!83 &1\nAudioClip:\n  m_StreamData: {path: old.resS, offset: 7, size: 4}\n";

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

    fn assert_target_unchanged(path: &Path, expected: &[u8]) {
        assert_eq!(fs::read(path).expect("target bytes"), expected);
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
        let layout = layout_from_locator(report.recovery()).expect("journal layout");
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
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("finalized recovery");
        assert_eq!(first, RecoveryOutcome::Committed(Box::new(report.clone())));

        let second = workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
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
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("workspace mismatch must block recovery");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::WorkspaceMismatch { .. })
        ));
        assert_eq!(fs::read_dir(&events).expect("events").count(), event_count);

        let outcome = workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("canonical recovery must remain available");
        assert_eq!(outcome, RecoveryOutcome::Committed(Box::new(report)));
    }

    #[test]
    fn published_recovery_rebuilds_and_installs_the_committed_baseline() {
        let (_directory, _path, mut reopened, report, _published) = published_restart_fixture();

        let outcome = reopened
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("recover published transaction");

        assert_eq!(
            outcome,
            RecoveryOutcome::Committed(Box::new(report.clone()))
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
            .recover_at(measured_report.recovery(), &mut measured)
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
            .recover_at(exact_report.recovery(), &mut exact)
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
            .recover_at(short_report.recovery(), &mut one_short)
            .expect_err("one-short recovery must fail");
        assert!(matches!(error, RecoveryError::Budget { .. }));
        assert_eq!(short_workspace.revision(), short_report.base_revision());
        assert_eq!(fs::read(short_path).expect("published target"), published);
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);
    }

    #[test]
    fn journaled_recovery_precharges_exact_budget_before_any_durable_write() {
        let (_measured_directory, _measured_path, mut measured_workspace, measured_report, _) =
            journaled_restart_fixture();
        let mut measured = AssetLoadBudget::default();
        measured_workspace
            .recover_at(measured_report.recovery(), &mut measured)
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
        let layout = layout_from_locator(report.recovery()).expect("journal layout");
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
        let event_count = fs::read_dir(&events).expect("events").count();
        assert!(!backup.exists());
        let mut one_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .expect("one-short budget");

        let error = workspace
            .recover_at(report.recovery(), &mut one_short)
            .expect_err("one-short journaled recovery must fail before mutation");

        assert!(matches!(error, RecoveryError::Budget { .. }));
        assert_eq!(workspace.revision(), report.base_revision());
        assert_eq!(fs::read(&path).expect("unchanged target"), target_before);
        assert_eq!(
            fs::read(&staging).expect("unchanged staging"),
            staging_before
        );
        assert!(!backup.exists());
        assert_eq!(fs::read_dir(&events).expect("events").count(), event_count);

        let mut exact = AssetLoadBudget::new(exact_limits).expect("exact budget");
        let outcome = workspace
            .recover_at(report.recovery(), &mut exact)
            .expect("exact recovery after one-short probe");
        assert_eq!(
            outcome,
            RecoveryOutcome::Committed(Box::new(report.clone()))
        );
        assert_eq!(exact.usage(), usage);
        assert_eq!(fs::read(path).expect("published target"), published);
    }

    #[test]
    fn journaled_recovery_captures_the_old_target_and_publishes_staging() {
        let (directory, path, mut reopened, report, published) = journaled_restart_fixture();
        let locator = PublicationTarget::in_place(directory.path())
            .expect("publication target")
            .recovery_locator(report.transaction());
        assert_eq!(&locator, report.recovery());

        let outcome = reopened
            .recover_at(&locator, &mut AssetLoadBudget::default())
            .expect("recover journaled transaction");

        assert_eq!(
            outcome,
            RecoveryOutcome::Committed(Box::new(report.clone()))
        );
        assert_eq!(fs::read(&path).expect("recovered target"), published);
        assert_eq!(reopened.revision(), report.committed_revision());
        assert_eq!(read_name(&reopened), "After");
    }

    #[test]
    fn explicit_abandon_rolls_back_an_unpublished_journaled_transaction() {
        let (_directory, path, mut reopened, report, _published) = journaled_restart_fixture();

        let outcome = reopened
            .abandon_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("explicit abandon must roll back journaled evidence");

        assert_eq!(
            outcome,
            RecoveryOutcome::RolledBack(report.recovery().clone())
        );
        assert_eq!(fs::read(&path).expect("restored target"), YAML);
        assert_eq!(reopened.revision(), report.base_revision());

        let layout = layout_from_locator(report.recovery()).expect("journal layout");
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
    fn explicit_abandon_refuses_a_sticky_forward_transaction() {
        let (_directory, path, mut reopened, report, published) = journaled_restart_fixture();
        let layout = layout_from_locator(report.recovery()).expect("journal layout");
        append_recovery_direction(&layout, RecoveryDirection::Forward);
        let original = fs::read(&path).expect("base target");
        let events = layout.events_directory();
        let event_count = fs::read_dir(&events).expect("events").count();

        let error = reopened
            .abandon_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("sticky forward transaction cannot be abandoned");

        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::InvalidEventSequence { .. })
        ));
        assert_eq!(fs::read(&path).expect("unchanged target"), original);
        assert_eq!(reopened.revision(), report.base_revision());
        assert_eq!(fs::read_dir(events).expect("events").count(), event_count);

        let outcome = reopened
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("ordinary forward recovery remains available");
        assert_eq!(
            outcome,
            RecoveryOutcome::Committed(Box::new(report.clone()))
        );
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
        let event_count = fs::read_dir(&events).expect("events").count();

        let mut wrong_workspace = AssetWorkspace::new().expect("wrong workspace");
        let error = wrong_workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("wrong workspace must block recovery");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::WorkspaceMismatch { .. })
        ));
        assert_eq!(fs::read_dir(&events).expect("events").count(), event_count);

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
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("wrong revision must block recovery");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::BaselineUnavailable { .. })
        ));
        assert_eq!(fs::read_dir(&events).expect("events").count(), event_count);
        assert_eq!(fs::read(&path).expect("base target"), YAML);

        let mut reopened = reopen_base_workspace(workspace_id, &path);
        let outcome = reopened
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("correct recovery remains available");
        assert_eq!(
            outcome,
            RecoveryOutcome::Committed(Box::new(report.clone()))
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

        let mut reopened = reopen_base_workspace(workspace_id, &path);
        let error = reopened
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
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
        let (_directory, path, mut workspace, report, published) = published_restart_fixture();
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

        let outcome = workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("owned corrupt target can be rolled back");

        assert_eq!(
            outcome,
            RecoveryOutcome::RolledBack(report.recovery().clone())
        );
        assert_eq!(fs::read(&path).expect("restored target"), YAML);
        assert_eq!(
            fs::read(staging).expect("preserved corrupt image"),
            tampered
        );
        assert_eq!(workspace.revision(), report.base_revision());
    }

    #[test]
    fn sticky_forward_rehomes_corrupt_new_inode_and_restores_old_target() {
        let (_directory, path, mut workspace, report, published) = published_restart_fixture();
        let (layout, artifact) = truncate_events_after(&report, |kind| {
            matches!(kind, JournalEventKind::PromotionIntent { .. })
        });
        append_recovery_direction(&layout, RecoveryDirection::Forward);
        let staging = artifact.staging().join_root(layout.directory());

        let mut tampered = published;
        tampered[0] ^= 1;
        fs::write(&path, &tampered).expect("same-inode target tamper");

        let error = workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
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
        let (_directory, path, mut workspace, report, published) = published_restart_fixture();
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

        let error = workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
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
        let (_directory, path, mut workspace, report, published) = published_restart_fixture();
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

        let error = workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
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
        .expect("restore base target identity");
        let mut reopened = reopen_base_workspace(workspace_id, &path);

        fs::remove_file(&path).expect("remove original target");
        fs::write(&path, YAML).expect("byte-identical replacement");
        let replacement_identity = observe_file_identity(&path).expect("replacement identity");
        assert_ne!(
            &replacement_identity,
            artifact.old_identity().expect("existing target identity")
        );

        let error = reopened
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
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
        let mut reopened = reopen_base_workspace(workspace_id, &path);
        capture_existing(
            &path,
            &backup,
            artifact.old_identity().expect("existing target identity"),
        )
        .expect("simulate completed backup rename");

        let outcome = reopened
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("recover captured backup");

        assert_eq!(
            outcome,
            RecoveryOutcome::Committed(Box::new(report.clone()))
        );
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

        fs::write(&path, YAML).expect("restore old target for reopen");
        let mut reopened = reopen_base_workspace(workspace_id, &path);
        fs::write(&path, &published).expect("simulate completed promotion rename");

        let outcome = reopened
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("recover promoted target");

        assert_eq!(
            outcome,
            RecoveryOutcome::Committed(Box::new(report.clone()))
        );
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
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
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
    fn tampered_target_is_blocked_without_replacing_the_target() {
        let (_directory, path, mut workspace, report) = committed_fixture();
        let published = fs::read(&path).expect("published target");
        let events = report.recovery().root().join("events");
        let event_count = fs::read_dir(&events).expect("events").count();
        let tampered = b"externally replaced bytes";
        fs::write(&path, tampered).expect("tamper target");

        let error = workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect_err("tampered target must block recovery");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::UnexpectedEvidence { .. })
        ));
        assert_target_unchanged(&path, tampered);
        assert_eq!(fs::read_dir(&events).expect("events").count(), event_count);

        fs::write(&path, published).expect("restore published target");
        let outcome = workspace
            .recover_at(report.recovery(), &mut AssetLoadBudget::default())
            .expect("canonical recovery must remain available");
        assert_eq!(outcome, RecoveryOutcome::Committed(Box::new(report)));
    }

    #[test]
    fn noncanonical_locator_is_blocked_before_any_target_write() {
        let (_directory, path, mut workspace, report) = committed_fixture();
        let original = fs::read(&path).expect("committed target");
        let malicious = RecoveryLocator::new(
            report.recovery().root().join("..").join("escape"),
            report.transaction(),
        );

        let error = workspace
            .recover_at(&malicious, &mut AssetLoadBudget::default())
            .expect_err("noncanonical locator must block");
        assert!(matches!(
            error.blocked_reason(),
            Some(RecoveryBlockedReason::InvalidLocator { .. })
        ));
        assert_target_unchanged(&path, &original);
    }
}
