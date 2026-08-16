//! Recovery of transactions with a canonical durable manifest.

use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError, DigestV1, WorkspaceRevision};

use crate::workspace::WorkspaceInstallationDigest;
use crate::workspace::portable_path::{PortablePathError, slash_key};
use crate::workspace::state::WorkspaceStateInstallOutcome;

use super::super::baseline::{
    BaselineBuildError, PreparedBaseline, RecoveryArtifactLocation, build_from_journal_with_images,
    read_artifact_image,
};
use super::super::journal::{
    Journal, JournalArtifact, JournalError, JournalEvent, JournalEventKind, JournalEventPlan,
    JournalPath, PlannedJournalEvent, matches_ordinal_journal_path,
};
use super::super::platform::{
    DirectoryIdentity, FileIdentity, JournalDirectory, SECURITY_METADATA_COPY_RESERVATION_BYTES,
    SecurityMetadataCopyReservation, SecurityMetadataError,
    capture_external_regular_in_journal_directory,
    copy_security_metadata_between_journal_directories, observe_directory_identity,
    open_journal_regular_in_directory, open_readonly_regular_in_parent, opened_file_identity,
    promote_journal_regular_to_external, reserve_security_metadata_copy,
};
use super::super::publication_protocol::{
    ArtifactObservation, ArtifactProgress, BaselineObservation, EntryEvidence, PreparedTransition,
    ProtocolBlock, ProtocolError, ProtocolEvent, ProtocolPlanError, PublicationAction,
    PublicationState, RecoveryDecision, RecoveryDirection, RecoveryIntent, RecoveryRequest,
    RecoveryStep, append_recovery_program, decide_recovery,
};
use super::super::{AssetWorkspace, CommitReport, RecoveryLocator, VerificationCharge};
#[cfg(test)]
use super::super::{
    test_record_verification_entry, test_record_verification_hash, test_run_publication_hook,
};
use super::{
    ObservationError, RecoveryBlockedReason, RecoveryError, RecoveryOutcome, RollbackReceipt,
    blocked, invalid_journal, io_reason, map_journal_error, map_observation_error,
    recovery_budget_error, recovery_vec,
};

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
) -> Result<PreparedBaseline, RecoveryError> {
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
            RecoveryArtifactLocation::Target
        } else if observation.staging == EntryEvidence::New {
            RecoveryArtifactLocation::Staging
        } else {
            return Err(blocked(
                locator,
                RecoveryBlockedReason::UnexpectedEvidence {
                    artifact: format!("artifact-{index:08}"),
                },
            ));
        };
        let image = read_artifact_image(journal, index, location, budget)
            .map_err(|error| map_baseline_error(locator, error))?;
        images.push(Some(image));
    }
    let expected = Arc::clone(workspace.state());
    build_from_journal_with_images(
        expected,
        journal,
        workspace.binary_adapter(),
        Some(&images),
        budget,
    )
    .map_err(|error| map_baseline_error(locator, error))
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

pub(super) fn recover_open_journal(
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
        .map_err(|error| map_journal_error(locator, error))?;
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
                .map_err(|error| map_journal_error(locator, error))?;
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
            test_run_publication_hook("before_recovery_execution");
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
                .map_err(|error| map_journal_error(locator, error))?;
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
            test_run_publication_hook("before_recovery_execution");
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
    artifact: &JournalPath,
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
            let backup = artifact
                .backup()
                .map(|backup| {
                    recovery_join(
                        journal.layout().directory(),
                        backup,
                        "recovery execution backup path",
                        budget,
                    )
                })
                .transpose()?;
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
        Err(error) => return Err(io_reason(error).into()),
    };
    let metadata = file.metadata().map_err(io_reason)?;
    let length = metadata.len();
    let identity = opened_file_identity(&file).map_err(io_reason)?;
    budget.consume_entries(1)?;
    budget.consume_bytes(length)?;
    #[cfg(test)]
    test_record_verification_hash(length);
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
        Err(error) => return Err(io_reason(error).into()),
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
        .checked_mul(SECURITY_METADATA_COPY_RESERVATION_BYTES)
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
    relative
        .join_root_budgeted(root, resource, budget)
        .map_err(map_recovery_join_error)
}

fn map_recovery_join_error(error: JournalError) -> ObservationError {
    match error {
        JournalError::Budget(error) => ObservationError::Budget(error),
        JournalError::Allocation {
            resource, message, ..
        } => ObservationError::Blocked(RecoveryBlockedReason::Io {
            message: format!("failed to reserve {resource}: {message}"),
        }),
        error => ObservationError::Blocked(invalid_journal(error.to_string())),
    }
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
    prebuilt_baseline: Option<PreparedBaseline>,
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
                    test_run_publication_hook("after_recovery_backup_replay");
                }
            }
            PublicationAction::PromotionIntent(ordinal) => {
                verify_recovery_promotion_intent(journal, observations, execution, ordinal)?;
            }
            PublicationAction::Promoted(ordinal) => {
                execute_recovery_promotion(journal, observations, execution, ordinal)?;
                #[cfg(test)]
                if !step.records_event() {
                    test_run_publication_hook("after_recovery_promotion_replay");
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
                test_run_publication_hook("before_recovery_rollback");
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
    prebuilt_baseline: Option<&PreparedBaseline>,
    expected: RecoveryBaselineExpectation,
) -> Result<(), ExecutionError> {
    #[cfg(test)]
    test_run_publication_hook("before_recovery_baseline_install");
    verify_published_artifacts(journal, observations, execution)?;
    let baseline = prebuilt_baseline.ok_or_else(|| {
        ExecutionError::Blocked(invalid_event(
            "recovery baseline was not prepared before execution",
        ))
    })?;
    match workspace.install_prepared_state(baseline.state()) {
        WorkspaceStateInstallOutcome::Installed | WorkspaceStateInstallOutcome::Unchanged => {
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
        WorkspaceStateInstallOutcome::Stale => Err(ExecutionError::Blocked(
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

fn protocol_journal_error(error: ProtocolError) -> JournalError {
    JournalError::InvalidEvent(error.to_string())
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
    test_record_verification_hash(expected_identity.length());
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
    test_record_verification_entry();
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
    test_record_verification_entry();
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
            test_record_verification_entry();
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
    test_record_verification_hash(expected_identity.length());
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
    Journal(#[from] JournalError),
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
            .map_err(|error| map_journal_error(locator, error))?;
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

fn map_baseline_error(locator: &RecoveryLocator, error: BaselineBuildError) -> RecoveryError {
    match error.into_budget() {
        Ok(source) => recovery_budget_error(locator, source),
        Err(BaselineBuildError::Revision { expected, actual }) => blocked(
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

fn map_execution_error(locator: &RecoveryLocator, error: ExecutionError) -> RecoveryError {
    match error {
        ExecutionError::Budget(source) => recovery_budget_error(locator, source),
        ExecutionError::Blocked(reason) => blocked(locator, reason),
        ExecutionError::Journal(error) => map_journal_error(locator, error),
        ExecutionError::Io(error) => blocked(locator, io_reason(error)),
    }
}

fn map_security_metadata_execution_error(error: SecurityMetadataError) -> ExecutionError {
    match error {
        SecurityMetadataError::Budget(source) => ExecutionError::Budget(source),
        SecurityMetadataError::Io(error) => ExecutionError::Io(error),
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    #[derive(Debug)]
    pub(in super::super) enum VerificationError {
        Blocked(RecoveryBlockedReason),
        Unexpected,
    }

    pub(in super::super) struct ObservedExecution {
        execution: RecoveryExecutionPlan,
        artifacts: Vec<ArtifactObservation>,
    }

    impl ObservedExecution {
        pub(in super::super) fn verify_published(
            &self,
            journal: &Journal,
        ) -> Result<(), VerificationError> {
            verify_published_artifacts(journal, &self.artifacts, &self.execution)
                .map_err(VerificationError::from)
        }

        pub(in super::super) fn verify_rolled_back(
            &self,
            journal: &Journal,
        ) -> Result<(), VerificationError> {
            verify_rolled_back_artifacts(journal, &self.artifacts, &self.execution)
                .map_err(VerificationError::from)
        }
    }

    impl From<ExecutionError> for VerificationError {
        fn from(error: ExecutionError) -> Self {
            match error {
                ExecutionError::Blocked(reason) => Self::Blocked(reason),
                _ => Self::Unexpected,
            }
        }
    }

    pub(in super::super) fn observe_execution_for_test(
        journal: &Journal,
    ) -> Result<ObservedExecution, ObservationError> {
        let (execution, artifacts) = observe_execution(journal, &mut AssetLoadBudget::default())?;
        Ok(ObservedExecution {
            execution,
            artifacts,
        })
    }

    pub(in super::super) fn planned_verification_charge(
        journal: &Journal,
        baseline: BaselineObservation,
        direction: RecoveryDirection,
        finalize_workspace: bool,
    ) -> Result<VerificationCharge, ObservationError> {
        let events = ObservedProtocol::from_journal(journal, &mut AssetLoadBudget::default())?;
        let (execution, artifacts) = observe_execution(journal, &mut AssetLoadBudget::default())?;
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
        )?;
        Ok(execution_verification_charge(
            journal,
            &execution,
            &observation.artifacts,
            &program.steps,
            direction,
        )?
        .charge)
    }

    pub(in super::super) fn rollback_outcome_for_test(report: &CommitReport) -> RecoveryOutcome {
        rollback_outcome(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
