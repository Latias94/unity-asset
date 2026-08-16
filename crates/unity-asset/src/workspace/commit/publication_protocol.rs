//! Pure publication state transitions shared by commit and recovery.
//!
//! This module owns durable publication semantics. It deliberately knows
//! nothing about paths, journal encoding, filesystem handles, or workspace
//! storage so those adapters cannot redefine the legal event language.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Sticky direction selected before recovery mutates publication evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryDirection {
    Forward,
    Rollback,
}

/// Path-independent durable action understood by the publication protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicationAction {
    StagingVerified,
    Journaled,
    BackupIntent(u32),
    BackupCaptured(u32),
    PromotionIntent(u32),
    Promoted(u32),
    Published,
    BaselineInstalled,
    Finalized,
    RecoveryDecision(RecoveryDirection),
    Abandoned,
}

impl PublicationAction {
    const fn is_forward_progress(self) -> bool {
        matches!(
            self,
            Self::StagingVerified
                | Self::Journaled
                | Self::BackupIntent(_)
                | Self::BackupCaptured(_)
                | Self::PromotionIntent(_)
                | Self::Promoted(_)
                | Self::Published
                | Self::BaselineInstalled
        )
    }
}

/// One recovery operation, separated from whether it advances durable state.
///
/// Completed filesystem actions remain safe to replay when their exact owned
/// inode has moved back to an earlier topology or its security metadata must
/// be restored. Replays never produce a second journal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryStep {
    Record(PublicationAction),
    Replay(PublicationAction),
}

impl RecoveryStep {
    #[must_use]
    pub(crate) const fn action(self) -> PublicationAction {
        match self {
            Self::Record(action) | Self::Replay(action) => action,
        }
    }

    #[must_use]
    pub(crate) const fn records_event(self) -> bool {
        matches!(self, Self::Record(_))
    }
}

/// Non-action records that still affect whether a journal prefix is legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtocolEvent {
    Action(PublicationAction),
    RecoveryBlocked,
    /// Read-only compatibility for a version-3 diagnostic record.
    LegacyMarker,
}

/// A transition validated before its durable record is appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreparedTransition(PublicationAction);

/// One artifact's immutable topology and accumulated durable progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArtifactProgress {
    has_backup: bool,
    backup_intent: bool,
    backup_captured: bool,
    promotion_intent: bool,
    promoted: bool,
}

impl ArtifactProgress {
    #[must_use]
    pub(crate) const fn new(has_backup: bool) -> Self {
        Self {
            has_backup,
            backup_intent: false,
            backup_captured: false,
            promotion_intent: false,
            promoted: false,
        }
    }

    #[must_use]
    pub(crate) const fn has_backup(self) -> bool {
        self.has_backup
    }

    #[must_use]
    pub(crate) const fn backup_intent(self) -> bool {
        self.backup_intent
    }

    #[must_use]
    pub(crate) const fn backup_captured(self) -> bool {
        self.backup_captured
    }

    #[must_use]
    pub(crate) const fn promotion_intent(self) -> bool {
        self.promotion_intent
    }

    #[must_use]
    pub(crate) const fn promoted(self) -> bool {
        self.promoted
    }
}

/// Durable logical state reconstructed by reducing the journal event chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicationState {
    staging_verified: bool,
    journaled: bool,
    artifacts: Vec<ArtifactProgress>,
    published: bool,
    baseline_installed: bool,
    abandoned: bool,
    finalized: bool,
    direction: Option<RecoveryDirection>,
    recovery_blocked: bool,
}

impl PublicationState {
    #[must_use]
    pub(crate) const fn new(artifacts: Vec<ArtifactProgress>) -> Self {
        Self {
            staging_verified: false,
            journaled: false,
            artifacts,
            published: false,
            baseline_installed: false,
            abandoned: false,
            finalized: false,
            direction: None,
            recovery_blocked: false,
        }
    }

    #[must_use]
    pub(crate) const fn staging_verified(&self) -> bool {
        self.staging_verified
    }

    #[must_use]
    pub(crate) const fn journaled(&self) -> bool {
        self.journaled
    }

    #[must_use]
    pub(crate) fn artifacts(&self) -> &[ArtifactProgress] {
        &self.artifacts
    }

    #[must_use]
    pub(crate) fn artifact(&self, ordinal: u32) -> Option<&ArtifactProgress> {
        usize::try_from(ordinal)
            .ok()
            .and_then(|index| self.artifacts.get(index))
    }

    #[must_use]
    pub(crate) const fn published(&self) -> bool {
        self.published
    }

    #[must_use]
    pub(crate) const fn baseline_installed(&self) -> bool {
        self.baseline_installed
    }

    #[must_use]
    pub(crate) const fn abandoned(&self) -> bool {
        self.abandoned
    }

    #[must_use]
    pub(crate) const fn finalized(&self) -> bool {
        self.finalized
    }

    #[must_use]
    pub(crate) const fn direction(&self) -> Option<RecoveryDirection> {
        self.direction
    }

    #[must_use]
    pub(crate) const fn recovery_blocked(&self) -> bool {
        self.recovery_blocked
    }

    pub(crate) fn validate(&self, event: ProtocolEvent) -> Result<(), ProtocolError> {
        if self.finalized && event != ProtocolEvent::LegacyMarker {
            return Err(ProtocolError::InvalidEventSequence(
                "an event follows Finalized",
            ));
        }
        if self.recovery_blocked && event != ProtocolEvent::LegacyMarker {
            return Err(ProtocolError::InvalidEventSequence(
                "an event follows RecoveryBlocked",
            ));
        }

        match event {
            ProtocolEvent::RecoveryBlocked | ProtocolEvent::LegacyMarker => Ok(()),
            ProtocolEvent::Action(action) => self.validate_action(action),
        }
    }

    pub(crate) fn apply(&mut self, event: ProtocolEvent) -> Result<(), ProtocolError> {
        self.validate(event)?;
        match event {
            ProtocolEvent::RecoveryBlocked => self.recovery_blocked = true,
            ProtocolEvent::LegacyMarker => {}
            ProtocolEvent::Action(action) => self.apply_validated_action(action),
        }
        Ok(())
    }

    pub(crate) fn prepare(
        &self,
        action: PublicationAction,
    ) -> Result<PreparedTransition, ProtocolError> {
        self.validate(ProtocolEvent::Action(action))?;
        Ok(PreparedTransition(action))
    }

    pub(crate) fn apply_prepared(&mut self, transition: PreparedTransition) {
        self.apply_validated_action(transition.0);
    }

    fn validate_action(&self, action: PublicationAction) -> Result<(), ProtocolError> {
        if self.direction == Some(RecoveryDirection::Rollback) && action.is_forward_progress() {
            return Err(ProtocolError::InvalidEventSequence(
                "a forward publication event follows a rollback decision",
            ));
        }

        match action {
            PublicationAction::StagingVerified => require_unset(
                self.staging_verified,
                "StagingVerified appears more than once",
            ),
            PublicationAction::Journaled => {
                if !self.staging_verified {
                    return Err(ProtocolError::InvalidEventSequence(
                        "Journaled precedes StagingVerified",
                    ));
                }
                require_unset(self.journaled, "Journaled appears more than once")
            }
            PublicationAction::BackupIntent(ordinal) => {
                self.require_artifact_turn(ordinal)?;
                let artifact = self.artifact_or_error(ordinal)?;
                if !artifact.has_backup {
                    return Err(ProtocolError::InvalidEventSequence(
                        "backup intent names an artifact without a backup",
                    ));
                }
                require_unset(
                    artifact.backup_intent,
                    "backup intent appears more than once",
                )
            }
            PublicationAction::BackupCaptured(ordinal) => {
                self.require_artifact_turn(ordinal)?;
                let artifact = self.artifact_or_error(ordinal)?;
                if !artifact.backup_intent {
                    return Err(ProtocolError::InvalidEventSequence(
                        "backup capture has no durable intent",
                    ));
                }
                require_unset(
                    artifact.backup_captured,
                    "backup capture appears more than once",
                )
            }
            PublicationAction::PromotionIntent(ordinal) => {
                self.require_artifact_turn(ordinal)?;
                let artifact = self.artifact_or_error(ordinal)?;
                if artifact.has_backup && !artifact.backup_captured {
                    return Err(ProtocolError::InvalidEventSequence(
                        "promotion intent precedes backup capture",
                    ));
                }
                require_unset(
                    artifact.promotion_intent,
                    "promotion intent appears more than once",
                )
            }
            PublicationAction::Promoted(ordinal) => {
                self.require_artifact_turn(ordinal)?;
                let artifact = self.artifact_or_error(ordinal)?;
                if !artifact.promotion_intent {
                    return Err(ProtocolError::InvalidEventSequence(
                        "promotion completion has no durable intent",
                    ));
                }
                require_unset(
                    artifact.promoted,
                    "promotion completion appears more than once",
                )
            }
            PublicationAction::Published => {
                if !self.journaled {
                    return Err(ProtocolError::InvalidEventSequence(
                        "Published precedes Journaled",
                    ));
                }
                if self.artifacts.iter().any(|artifact| !artifact.promoted) {
                    return Err(ProtocolError::InvalidEventSequence(
                        "Published precedes an artifact promotion",
                    ));
                }
                require_unset(self.published, "Published appears more than once")
            }
            PublicationAction::BaselineInstalled => {
                if !self.published || self.abandoned {
                    return Err(ProtocolError::InvalidEventSequence(
                        "BaselineInstalled does not follow a published transaction",
                    ));
                }
                require_unset(
                    self.baseline_installed,
                    "BaselineInstalled appears more than once",
                )
            }
            PublicationAction::Finalized => {
                if !self.baseline_installed && !self.abandoned {
                    return Err(ProtocolError::InvalidEventSequence(
                        "Finalized has neither an installed baseline nor rollback",
                    ));
                }
                require_unset(self.finalized, "Finalized appears more than once")
            }
            PublicationAction::RecoveryDecision(direction) => {
                if direction == RecoveryDirection::Rollback
                    && (self.published || self.baseline_installed || self.abandoned)
                {
                    return Err(ProtocolError::InvalidEventSequence(
                        "a rollback decision follows completed forward publication",
                    ));
                }
                if self.direction.is_some() {
                    return Err(ProtocolError::ConflictingDecision);
                }
                Ok(())
            }
            PublicationAction::Abandoned => {
                if self.direction != Some(RecoveryDirection::Rollback)
                    || self.published
                    || self.baseline_installed
                {
                    return Err(ProtocolError::InvalidEventSequence(
                        "Abandoned has no valid rollback decision",
                    ));
                }
                require_unset(self.abandoned, "Abandoned appears more than once")
            }
        }
    }

    fn apply_validated_action(&mut self, action: PublicationAction) {
        match action {
            PublicationAction::StagingVerified => self.staging_verified = true,
            PublicationAction::Journaled => self.journaled = true,
            PublicationAction::BackupIntent(ordinal) => {
                self.artifact_mut_validated(ordinal).backup_intent = true;
            }
            PublicationAction::BackupCaptured(ordinal) => {
                self.artifact_mut_validated(ordinal).backup_captured = true;
            }
            PublicationAction::PromotionIntent(ordinal) => {
                self.artifact_mut_validated(ordinal).promotion_intent = true;
            }
            PublicationAction::Promoted(ordinal) => {
                self.artifact_mut_validated(ordinal).promoted = true;
            }
            PublicationAction::Published => self.published = true,
            PublicationAction::BaselineInstalled => self.baseline_installed = true,
            PublicationAction::Finalized => self.finalized = true,
            PublicationAction::RecoveryDecision(direction) => self.direction = Some(direction),
            PublicationAction::Abandoned => self.abandoned = true,
        }
    }

    fn require_artifact_turn(&self, ordinal: u32) -> Result<(), ProtocolError> {
        if !self.journaled {
            return Err(ProtocolError::InvalidEventSequence(
                "an artifact event precedes Journaled",
            ));
        }
        let index = usize::try_from(ordinal).map_err(|_| ProtocolError::ArtifactOutOfRange)?;
        if index >= self.artifacts.len() {
            return Err(ProtocolError::ArtifactOutOfRange);
        }
        if self.artifacts[..index]
            .iter()
            .any(|artifact| !artifact.promoted)
        {
            return Err(ProtocolError::InvalidEventSequence(
                "an artifact event precedes an earlier artifact promotion",
            ));
        }
        Ok(())
    }

    fn artifact_or_error(&self, ordinal: u32) -> Result<&ArtifactProgress, ProtocolError> {
        self.artifact(ordinal)
            .ok_or(ProtocolError::ArtifactOutOfRange)
    }

    fn artifact_mut_validated(&mut self, ordinal: u32) -> &mut ArtifactProgress {
        let index = usize::try_from(ordinal).expect("validated artifact ordinal fits usize");
        &mut self.artifacts[index]
    }
}

/// Semantic failure while reducing or planning publication events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ProtocolError {
    #[error("{0}")]
    InvalidEventSequence(&'static str),
    #[error("an artifact event ordinal is outside the publication manifest")]
    ArtifactOutOfRange,
    #[error("the journal contains conflicting recovery decisions")]
    ConflictingDecision,
    #[error("publication artifact ordinals are not contiguous")]
    NonContiguousArtifacts,
}

/// Classified evidence for one transaction-owned filesystem location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryEvidence {
    Missing,
    Old,
    New,
    CorruptOld,
    CorruptNew,
    Unexpected,
}

/// Filesystem evidence for one publication artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArtifactObservation {
    pub(crate) target: EntryEvidence,
    pub(crate) staging: EntryEvidence,
    pub(crate) backup: EntryEvidence,
    pub(crate) had_original: bool,
}

impl ArtifactObservation {
    #[must_use]
    pub(crate) const fn can_forward(self) -> bool {
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

    #[must_use]
    pub(crate) const fn can_rollback(self) -> bool {
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

    #[must_use]
    pub(crate) fn is_published(self) -> bool {
        self.target == EntryEvidence::New
            && self.staging == EntryEvidence::Missing
            && if self.had_original {
                self.backup == EntryEvidence::Old
            } else {
                self.backup == EntryEvidence::Missing
            }
    }

    #[must_use]
    pub(crate) fn is_rolled_back(self) -> bool {
        let target_matches = if self.had_original {
            self.target == EntryEvidence::Old
        } else {
            self.target == EntryEvidence::Missing
        };
        target_matches
            && self.staging != EntryEvidence::Unexpected
            && self.backup == EntryEvidence::Missing
    }

    #[must_use]
    pub(crate) fn contains_unexpected(self) -> bool {
        self.target == EntryEvidence::Unexpected
            || self.staging == EntryEvidence::Unexpected
            || self.backup == EntryEvidence::Unexpected
    }

    #[must_use]
    pub(crate) fn contains_corrupt_owned(self) -> bool {
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

    #[must_use]
    pub(crate) fn has_repairable_owned_corruption(self) -> bool {
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

/// Relationship between an attached workspace and the journal base state.
///
/// A revision can intentionally exclude physical source bindings, so matching
/// the committed revision does not prove that the committed state is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaselineObservation {
    Base,
    NotBase,
    Detached,
}

/// Recovery operation requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryIntent {
    Resume,
    Abandon,
}

/// Protocol-level reason that recovery cannot choose a safe action program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtocolBlock {
    PreviousRecoveryBlocked,
    UnexpectedEvidence { artifact: usize },
    InvalidEventSequence(&'static str),
}

/// Failure to derive a complete pre-encoded durable action program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ProtocolPlanError {
    #[error("{0}")]
    InvalidState(&'static str),
    #[error("artifact {artifact} has evidence outside the selected recovery direction")]
    UnexpectedEvidence { artifact: usize },
    #[error("publication artifact ordinal overflowed")]
    ArtifactOrdinalOverflow,
}

/// Inputs used to select the sticky recovery direction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecoveryRequest<'a> {
    pub(crate) intent: RecoveryIntent,
    pub(crate) state: &'a PublicationState,
    pub(crate) artifacts: &'a [ArtifactObservation],
    pub(crate) baseline: BaselineObservation,
}

/// Pure recovery decision produced before any durable or filesystem action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryDecision {
    Forward,
    Rollback,
    Blocked(ProtocolBlock),
}

/// Chooses a deterministic recovery direction without touching persistent state.
#[must_use]
pub(crate) fn decide_recovery(request: RecoveryRequest<'_>) -> RecoveryDecision {
    if request.state.recovery_blocked() {
        return RecoveryDecision::Blocked(ProtocolBlock::PreviousRecoveryBlocked);
    }
    if let Some((artifact, _)) = request
        .artifacts
        .iter()
        .enumerate()
        .find(|(_, artifact)| artifact.contains_unexpected())
    {
        return RecoveryDecision::Blocked(ProtocolBlock::UnexpectedEvidence { artifact });
    }

    if request.state.abandoned() {
        return if request
            .artifacts
            .iter()
            .all(|artifact| artifact.is_rolled_back())
            && matches!(
                request.baseline,
                BaselineObservation::Base | BaselineObservation::Detached
            ) {
            RecoveryDecision::Rollback
        } else {
            RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                "an abandoned transaction is not fully rolled back",
            ))
        };
    }

    if request.state.published() || request.state.baseline_installed() || request.state.finalized()
    {
        if request.intent == RecoveryIntent::Abandon {
            return RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                "a published or finalized transaction cannot be explicitly abandoned",
            ));
        }
        if let Some((artifact, _)) = request
            .artifacts
            .iter()
            .enumerate()
            .find(|(_, artifact)| artifact.contains_corrupt_owned())
        {
            return RecoveryDecision::Blocked(ProtocolBlock::UnexpectedEvidence { artifact });
        }
        if !request
            .artifacts
            .iter()
            .all(|artifact| artifact.is_published())
        {
            return RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                "a published transaction does not retain every new artifact",
            ));
        }
        return RecoveryDecision::Forward;
    }

    if request.intent == RecoveryIntent::Abandon {
        return decide_abandon(request);
    }

    match request.state.direction() {
        Some(RecoveryDirection::Forward) => {
            if request
                .artifacts
                .iter()
                .all(|artifact| artifact.can_forward())
            {
                RecoveryDecision::Forward
            } else {
                RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                    "the sticky forward decision no longer has complete evidence",
                ))
            }
        }
        Some(RecoveryDirection::Rollback) => {
            if request
                .artifacts
                .iter()
                .all(|artifact| artifact.can_rollback())
            {
                RecoveryDecision::Rollback
            } else {
                RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                    "the sticky rollback decision no longer has complete evidence",
                ))
            }
        }
        None if request
            .artifacts
            .iter()
            .all(|artifact| artifact.can_forward()) =>
        {
            RecoveryDecision::Forward
        }
        None if request
            .artifacts
            .iter()
            .all(|artifact| artifact.can_rollback()) =>
        {
            RecoveryDecision::Rollback
        }
        None => {
            if let Some((artifact, _)) = request
                .artifacts
                .iter()
                .enumerate()
                .find(|(_, artifact)| artifact.contains_corrupt_owned())
            {
                RecoveryDecision::Blocked(ProtocolBlock::UnexpectedEvidence { artifact })
            } else {
                RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                    "neither forward publication nor rollback has complete evidence",
                ))
            }
        }
    }
}

fn decide_abandon(request: RecoveryRequest<'_>) -> RecoveryDecision {
    if request.state.direction() == Some(RecoveryDirection::Forward) {
        return RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
            "a transaction with a sticky forward decision cannot be abandoned",
        ));
    }
    if !matches!(
        request.baseline,
        BaselineObservation::Base | BaselineObservation::Detached
    ) {
        return RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
            "explicit abandon requires the workspace base revision",
        ));
    }
    if request
        .artifacts
        .iter()
        .all(|artifact| artifact.can_rollback())
    {
        RecoveryDecision::Rollback
    } else {
        RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
            "transaction evidence cannot be safely rolled back for explicit abandon",
        ))
    }
}

/// Appends the only legal live-commit action sequence to preallocated storage.
pub(crate) fn append_commit_program(
    artifact_count: usize,
    mut artifact: impl FnMut(usize) -> (u32, bool),
    actions: &mut Vec<PublicationAction>,
) -> Result<(), ProtocolError> {
    actions.push(PublicationAction::StagingVerified);
    actions.push(PublicationAction::Journaled);
    for index in 0..artifact_count {
        let expected = u32::try_from(index).map_err(|_| ProtocolError::NonContiguousArtifacts)?;
        let (ordinal, has_backup) = artifact(index);
        if ordinal != expected {
            return Err(ProtocolError::NonContiguousArtifacts);
        }
        if has_backup {
            actions.push(PublicationAction::BackupIntent(ordinal));
            actions.push(PublicationAction::BackupCaptured(ordinal));
        }
        actions.push(PublicationAction::PromotionIntent(ordinal));
        actions.push(PublicationAction::Promoted(ordinal));
    }
    actions.push(PublicationAction::Published);
    actions.push(PublicationAction::BaselineInstalled);
    actions.push(PublicationAction::Finalized);
    Ok(())
}

/// Appends the durable transitions and idempotent physical replays for recovery.
pub(crate) fn append_recovery_program(
    state: &PublicationState,
    artifacts: &[ArtifactObservation],
    direction: RecoveryDirection,
    finalize_workspace: bool,
    steps: &mut Vec<RecoveryStep>,
) -> Result<(), ProtocolPlanError> {
    if state.artifacts().len() != artifacts.len() {
        return Err(ProtocolPlanError::InvalidState(
            "recovery evidence does not cover every protocol artifact",
        ));
    }
    if state.finalized() {
        let terminal_matches = match direction {
            RecoveryDirection::Forward => !state.abandoned(),
            RecoveryDirection::Rollback => state.abandoned(),
        };
        return if terminal_matches {
            Ok(())
        } else {
            Err(ProtocolPlanError::InvalidState(
                "selected recovery direction conflicts with the finalized outcome",
            ))
        };
    }
    if direction == RecoveryDirection::Rollback && (state.published() || state.baseline_installed())
    {
        return Err(ProtocolPlanError::InvalidState(
            "rollback cannot follow completed forward publication",
        ));
    }
    if direction == RecoveryDirection::Forward && state.abandoned() {
        return Err(ProtocolPlanError::InvalidState(
            "forward publication cannot follow an abandoned transaction",
        ));
    }
    match state.direction() {
        Some(existing) if existing != direction => {
            return Err(ProtocolPlanError::InvalidState(
                "selected recovery direction conflicts with the durable decision",
            ));
        }
        Some(_) => {}
        None => steps.push(RecoveryStep::Record(PublicationAction::RecoveryDecision(
            direction,
        ))),
    }

    if direction == RecoveryDirection::Rollback {
        if !state.abandoned() {
            steps.push(RecoveryStep::Record(PublicationAction::Abandoned));
        }
        if !state.finalized() {
            steps.push(RecoveryStep::Record(PublicationAction::Finalized));
        }
        return Ok(());
    }

    if !state.staging_verified() {
        steps.push(RecoveryStep::Record(PublicationAction::StagingVerified));
    }
    if !state.journaled() {
        steps.push(RecoveryStep::Record(PublicationAction::Journaled));
    }
    for (index, (artifact, progress)) in artifacts.iter().zip(state.artifacts()).enumerate() {
        let ordinal =
            u32::try_from(index).map_err(|_| ProtocolPlanError::ArtifactOrdinalOverflow)?;
        if artifact.had_original != progress.has_backup() {
            return Err(ProtocolPlanError::InvalidState(
                "artifact topology changed after journal replay",
            ));
        }

        if artifact.is_published() {
            if progress.has_backup() && !progress.backup_captured() {
                return Err(ProtocolPlanError::InvalidState(
                    "promoted replacement has no captured backup event",
                ));
            }
            if !progress.promotion_intent() {
                return Err(ProtocolPlanError::InvalidState(
                    "promoted target has no durable intent",
                ));
            }
            if !progress.promoted() {
                steps.push(RecoveryStep::Record(PublicationAction::Promoted(ordinal)));
            }
            continue;
        }

        if progress.has_backup() {
            match (artifact.target, artifact.staging, artifact.backup) {
                (EntryEvidence::Old, EntryEvidence::New, EntryEvidence::Missing) => {
                    if !progress.backup_intent() {
                        steps.push(RecoveryStep::Record(PublicationAction::BackupIntent(
                            ordinal,
                        )));
                    }
                }
                (EntryEvidence::Missing, EntryEvidence::New, EntryEvidence::Old) => {
                    if !progress.backup_intent() {
                        return Err(ProtocolPlanError::InvalidState(
                            "captured backup has no durable intent",
                        ));
                    }
                }
                _ => {
                    return Err(ProtocolPlanError::UnexpectedEvidence { artifact: index });
                }
            }
            let backup_capture = PublicationAction::BackupCaptured(ordinal);
            steps.push(if progress.backup_captured() {
                RecoveryStep::Replay(backup_capture)
            } else {
                RecoveryStep::Record(backup_capture)
            });
        } else if !matches!(
            (artifact.target, artifact.staging, artifact.backup),
            (
                EntryEvidence::Missing,
                EntryEvidence::New,
                EntryEvidence::Missing
            )
        ) {
            return Err(ProtocolPlanError::UnexpectedEvidence { artifact: index });
        }

        if !progress.promotion_intent() {
            steps.push(RecoveryStep::Record(PublicationAction::PromotionIntent(
                ordinal,
            )));
        }
        let promotion = PublicationAction::Promoted(ordinal);
        steps.push(if progress.promoted() {
            RecoveryStep::Replay(promotion)
        } else {
            RecoveryStep::Record(promotion)
        });
    }
    if !state.published() {
        steps.push(RecoveryStep::Record(PublicationAction::Published));
    }
    if finalize_workspace {
        if !state.baseline_installed() {
            steps.push(RecoveryStep::Record(PublicationAction::BaselineInstalled));
        }
        if !state.finalized() {
            steps.push(RecoveryStep::Record(PublicationAction::Finalized));
        }
    }
    Ok(())
}

fn require_unset(value: bool, message: &'static str) -> Result<(), ProtocolError> {
    if value {
        return Err(ProtocolError::InvalidEventSequence(message));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(topology: &[bool]) -> PublicationState {
        PublicationState::new(
            topology
                .iter()
                .copied()
                .map(ArtifactProgress::new)
                .collect(),
        )
    }

    fn commit_program(topology: &[bool]) -> Vec<PublicationAction> {
        let mut actions = Vec::new();
        append_commit_program(
            topology.len(),
            |index| (u32::try_from(index).unwrap(), topology[index]),
            &mut actions,
        )
        .unwrap();
        actions
    }

    fn recorded(actions: impl IntoIterator<Item = PublicationAction>) -> Vec<RecoveryStep> {
        actions.into_iter().map(RecoveryStep::Record).collect()
    }

    fn recorded_actions(steps: &[RecoveryStep]) -> Vec<PublicationAction> {
        steps
            .iter()
            .copied()
            .filter(|step| step.records_event())
            .map(RecoveryStep::action)
            .collect()
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

    fn recovery_state(artifacts: &[ArtifactObservation]) -> PublicationState {
        PublicationState::new(
            artifacts
                .iter()
                .map(|artifact| ArtifactProgress::new(artifact.had_original))
                .collect(),
        )
    }

    fn initial_observations(topology: &[bool]) -> Vec<ArtifactObservation> {
        topology
            .iter()
            .map(|has_backup| {
                if *has_backup {
                    existing(
                        EntryEvidence::Old,
                        EntryEvidence::New,
                        EntryEvidence::Missing,
                    )
                } else {
                    absent(EntryEvidence::Missing, EntryEvidence::New)
                }
            })
            .collect()
    }

    fn observations_after_prefix(
        topology: &[bool],
        prefix: &[PublicationAction],
    ) -> Vec<ArtifactObservation> {
        let mut observations = initial_observations(topology);
        for action in prefix {
            match *action {
                PublicationAction::BackupCaptured(ordinal) => {
                    let artifact = &mut observations[usize::try_from(ordinal).unwrap()];
                    artifact.target = EntryEvidence::Missing;
                    artifact.backup = EntryEvidence::Old;
                }
                PublicationAction::Promoted(ordinal) => {
                    let artifact = &mut observations[usize::try_from(ordinal).unwrap()];
                    artifact.target = EntryEvidence::New;
                    artifact.staging = EntryEvidence::Missing;
                }
                _ => {}
            }
        }
        observations
    }

    fn recovery_decision(
        state: &PublicationState,
        artifacts: &[ArtifactObservation],
        intent: RecoveryIntent,
        baseline: BaselineObservation,
    ) -> RecoveryDecision {
        decide_recovery(RecoveryRequest {
            intent,
            state,
            artifacts,
            baseline,
        })
    }

    fn mark_published(state: &mut PublicationState, topology: &[bool]) {
        for action in commit_program(topology) {
            state.apply(ProtocolEvent::Action(action)).unwrap();
            if action == PublicationAction::Published {
                break;
            }
        }
    }

    #[test]
    fn every_live_commit_prefix_is_legal() {
        let actions = commit_program(&[true, false]);
        for prefix_length in 0..=actions.len() {
            let mut state = state(&[true, false]);
            for action in actions.iter().copied().take(prefix_length) {
                state.apply(ProtocolEvent::Action(action)).unwrap();
            }
        }
    }

    #[test]
    fn every_live_commit_prefix_plans_the_exact_recovery_suffix() {
        for topology in [
            vec![true],
            vec![false],
            vec![true, false],
            vec![false, true],
        ] {
            let actions = commit_program(&topology);
            for prefix_length in 0..=actions.len() {
                let mut state = state(&topology);
                for action in actions.iter().copied().take(prefix_length) {
                    state.apply(ProtocolEvent::Action(action)).unwrap();
                }
                let artifacts = observations_after_prefix(&topology, &actions[..prefix_length]);
                let mut recovery = Vec::new();
                append_recovery_program(
                    &state,
                    &artifacts,
                    RecoveryDirection::Forward,
                    true,
                    &mut recovery,
                )
                .unwrap();

                let expected = if prefix_length == actions.len() {
                    Vec::new()
                } else {
                    let mut expected = vec![PublicationAction::RecoveryDecision(
                        RecoveryDirection::Forward,
                    )];
                    expected.extend_from_slice(&actions[prefix_length..]);
                    expected
                };
                assert_eq!(
                    recorded_actions(&recovery),
                    expected,
                    "topology {topology:?}, prefix {prefix_length}"
                );
                for step in recovery {
                    if step.records_event() {
                        state.apply(ProtocolEvent::Action(step.action())).unwrap();
                    }
                }
                assert!(state.finalized());
                assert!(state.published());
            }
        }
    }

    #[test]
    fn recovery_plans_completion_after_intent_side_effect_windows() {
        let topology = [true, false];
        let actions = commit_program(&topology);
        for intent in [
            PublicationAction::BackupIntent(0),
            PublicationAction::PromotionIntent(0),
            PublicationAction::PromotionIntent(1),
        ] {
            let prefix_length = actions.iter().position(|action| *action == intent).unwrap() + 1;
            let mut state = state(&topology);
            for action in actions.iter().copied().take(prefix_length) {
                state.apply(ProtocolEvent::Action(action)).unwrap();
            }
            let mut artifacts = observations_after_prefix(&topology, &actions[..prefix_length]);
            let ordinal = match intent {
                PublicationAction::BackupIntent(ordinal)
                | PublicationAction::PromotionIntent(ordinal) => ordinal,
                _ => unreachable!(),
            };
            let artifact = &mut artifacts[usize::try_from(ordinal).unwrap()];
            match intent {
                PublicationAction::BackupIntent(_) => {
                    artifact.target = EntryEvidence::Missing;
                    artifact.backup = EntryEvidence::Old;
                }
                PublicationAction::PromotionIntent(_) => {
                    artifact.target = EntryEvidence::New;
                    artifact.staging = EntryEvidence::Missing;
                }
                _ => unreachable!(),
            }

            let mut recovery = Vec::new();
            append_recovery_program(
                &state,
                &artifacts,
                RecoveryDirection::Forward,
                true,
                &mut recovery,
            )
            .unwrap();
            let mut expected = vec![PublicationAction::RecoveryDecision(
                RecoveryDirection::Forward,
            )];
            expected.extend_from_slice(&actions[prefix_length..]);
            assert_eq!(recovery, recorded(expected));
        }
    }

    #[test]
    fn live_commit_program_reaches_one_final_state() {
        let actions = commit_program(&[true, false]);
        let mut state = state(&[true, false]);
        for action in actions {
            state.apply(ProtocolEvent::Action(action)).unwrap();
        }

        assert!(state.staging_verified());
        assert!(state.journaled());
        assert!(state.artifacts().iter().all(|artifact| artifact.promoted()));
        assert!(state.published());
        assert!(state.baseline_installed());
        assert!(state.finalized());
        assert_eq!(state.direction(), None);
    }

    #[test]
    fn artifact_order_and_topology_are_enforced() {
        let mut replacement = state(&[true, false]);
        replacement
            .apply(ProtocolEvent::Action(PublicationAction::StagingVerified))
            .unwrap();
        replacement
            .apply(ProtocolEvent::Action(PublicationAction::Journaled))
            .unwrap();

        assert_eq!(
            replacement.apply(ProtocolEvent::Action(PublicationAction::PromotionIntent(0))),
            Err(ProtocolError::InvalidEventSequence(
                "promotion intent precedes backup capture"
            ))
        );
        assert_eq!(
            replacement.apply(ProtocolEvent::Action(PublicationAction::PromotionIntent(1))),
            Err(ProtocolError::InvalidEventSequence(
                "an artifact event precedes an earlier artifact promotion"
            ))
        );

        let mut absent = state(&[false]);
        absent
            .apply(ProtocolEvent::Action(PublicationAction::StagingVerified))
            .unwrap();
        absent
            .apply(ProtocolEvent::Action(PublicationAction::Journaled))
            .unwrap();
        assert_eq!(
            absent.apply(ProtocolEvent::Action(PublicationAction::BackupIntent(0))),
            Err(ProtocolError::InvalidEventSequence(
                "backup intent names an artifact without a backup"
            ))
        );
    }

    #[test]
    fn published_requires_journaled_even_without_artifacts() {
        let mut state = state(&[]);
        assert!(matches!(
            state.apply(ProtocolEvent::Action(PublicationAction::Published)),
            Err(ProtocolError::InvalidEventSequence(_))
        ));
    }

    #[test]
    fn rollback_direction_is_sticky_and_blocks_forward_progress() {
        let mut state = state(&[false]);
        state
            .apply(ProtocolEvent::Action(PublicationAction::RecoveryDecision(
                RecoveryDirection::Rollback,
            )))
            .unwrap();

        assert!(matches!(
            state.apply(ProtocolEvent::Action(PublicationAction::StagingVerified)),
            Err(ProtocolError::InvalidEventSequence(_))
        ));
        assert_eq!(
            state.apply(ProtocolEvent::Action(PublicationAction::RecoveryDecision(
                RecoveryDirection::Forward
            ))),
            Err(ProtocolError::ConflictingDecision)
        );
    }

    #[test]
    fn published_is_a_forward_only_boundary() {
        let mut state = state(&[]);
        for action in [
            PublicationAction::StagingVerified,
            PublicationAction::Journaled,
            PublicationAction::Published,
        ] {
            state.apply(ProtocolEvent::Action(action)).unwrap();
        }

        assert!(matches!(
            state.apply(ProtocolEvent::Action(PublicationAction::RecoveryDecision(
                RecoveryDirection::Rollback
            ))),
            Err(ProtocolError::InvalidEventSequence(_))
        ));
        assert!(matches!(
            append_recovery_program(
                &state,
                &[],
                RecoveryDirection::Rollback,
                true,
                &mut Vec::new(),
            ),
            Err(ProtocolPlanError::InvalidState(
                "rollback cannot follow completed forward publication"
            ))
        ));
    }

    #[test]
    fn rollback_finalization_requires_a_sticky_decision() {
        let mut state = state(&[]);
        assert!(matches!(
            state.apply(ProtocolEvent::Action(PublicationAction::Abandoned)),
            Err(ProtocolError::InvalidEventSequence(_))
        ));

        state
            .apply(ProtocolEvent::Action(PublicationAction::RecoveryDecision(
                RecoveryDirection::Rollback,
            )))
            .unwrap();
        state
            .apply(ProtocolEvent::Action(PublicationAction::Abandoned))
            .unwrap();
        state
            .apply(ProtocolEvent::Action(PublicationAction::Finalized))
            .unwrap();
        assert!(state.abandoned());
        assert!(state.finalized());
    }

    #[test]
    fn terminal_records_reject_later_events() {
        let mut blocked = state(&[]);
        blocked.apply(ProtocolEvent::RecoveryBlocked).unwrap();
        assert!(matches!(
            blocked.apply(ProtocolEvent::Action(PublicationAction::RecoveryDecision(
                RecoveryDirection::Forward
            ))),
            Err(ProtocolError::InvalidEventSequence(_))
        ));

        let mut finalized = state(&[]);
        for action in [
            PublicationAction::RecoveryDecision(RecoveryDirection::Rollback),
            PublicationAction::Abandoned,
            PublicationAction::Finalized,
        ] {
            finalized.apply(ProtocolEvent::Action(action)).unwrap();
        }
        assert!(matches!(
            finalized.apply(ProtocolEvent::RecoveryBlocked),
            Err(ProtocolError::InvalidEventSequence(_))
        ));
        finalized.apply(ProtocolEvent::LegacyMarker).unwrap();
    }

    #[test]
    fn recovery_direction_matrix_is_deterministic() {
        let cases = [
            (
                vec![
                    existing(
                        EntryEvidence::Old,
                        EntryEvidence::New,
                        EntryEvidence::Missing,
                    ),
                    absent(EntryEvidence::Missing, EntryEvidence::New),
                ],
                RecoveryDecision::Forward,
            ),
            (
                vec![
                    existing(
                        EntryEvidence::New,
                        EntryEvidence::Missing,
                        EntryEvidence::Old,
                    ),
                    absent(EntryEvidence::Missing, EntryEvidence::New),
                ],
                RecoveryDecision::Forward,
            ),
            (
                vec![
                    existing(
                        EntryEvidence::New,
                        EntryEvidence::Missing,
                        EntryEvidence::Old,
                    ),
                    absent(EntryEvidence::Missing, EntryEvidence::Missing),
                ],
                RecoveryDecision::Rollback,
            ),
            (
                vec![existing(
                    EntryEvidence::Unexpected,
                    EntryEvidence::New,
                    EntryEvidence::Missing,
                )],
                RecoveryDecision::Blocked(ProtocolBlock::UnexpectedEvidence { artifact: 0 }),
            ),
            (
                vec![existing(
                    EntryEvidence::Old,
                    EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                )],
                RecoveryDecision::Blocked(ProtocolBlock::UnexpectedEvidence { artifact: 0 }),
            ),
            (
                vec![existing(
                    EntryEvidence::CorruptNew,
                    EntryEvidence::Missing,
                    EntryEvidence::Old,
                )],
                RecoveryDecision::Rollback,
            ),
            (
                vec![existing(
                    EntryEvidence::Missing,
                    EntryEvidence::New,
                    EntryEvidence::CorruptOld,
                )],
                RecoveryDecision::Rollback,
            ),
            (
                vec![existing(
                    EntryEvidence::New,
                    EntryEvidence::Missing,
                    EntryEvidence::Missing,
                )],
                RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                    "neither forward publication nor rollback has complete evidence",
                )),
            ),
        ];

        for (artifacts, expected) in cases {
            let state = recovery_state(&artifacts);
            assert_eq!(
                recovery_decision(
                    &state,
                    &artifacts,
                    RecoveryIntent::Resume,
                    BaselineObservation::Base,
                ),
                expected
            );
        }
    }

    #[test]
    fn sticky_direction_and_published_boundary_override_heuristics() {
        let artifacts = vec![existing(
            EntryEvidence::New,
            EntryEvidence::Missing,
            EntryEvidence::Old,
        )];
        let mut rollback = recovery_state(&artifacts);
        rollback
            .apply(ProtocolEvent::Action(PublicationAction::RecoveryDecision(
                RecoveryDirection::Rollback,
            )))
            .unwrap();
        assert_eq!(
            recovery_decision(
                &rollback,
                &artifacts,
                RecoveryIntent::Resume,
                BaselineObservation::Base,
            ),
            RecoveryDecision::Rollback
        );

        let mut published = recovery_state(&artifacts);
        mark_published(&mut published, &[true]);
        assert_eq!(
            recovery_decision(
                &published,
                &artifacts,
                RecoveryIntent::Resume,
                BaselineObservation::Base,
            ),
            RecoveryDecision::Forward
        );
        assert_eq!(
            recovery_decision(
                &published,
                &artifacts,
                RecoveryIntent::Abandon,
                BaselineObservation::Base,
            ),
            RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                "a published or finalized transaction cannot be explicitly abandoned",
            ))
        );
    }

    #[test]
    fn finalized_receipt_is_redelivered_without_repeating_side_effects() {
        let artifacts = vec![absent(EntryEvidence::New, EntryEvidence::Missing)];
        let mut state = recovery_state(&artifacts);
        for action in commit_program(&[false]) {
            state.apply(ProtocolEvent::Action(action)).unwrap();
        }

        for baseline in [
            BaselineObservation::Base,
            BaselineObservation::NotBase,
            BaselineObservation::Detached,
        ] {
            assert_eq!(
                recovery_decision(&state, &artifacts, RecoveryIntent::Resume, baseline),
                RecoveryDecision::Forward
            );
        }
    }

    #[test]
    fn previous_block_is_terminal_for_direction_selection() {
        let mut state = state(&[]);
        state.apply(ProtocolEvent::RecoveryBlocked).unwrap();
        let decision = decide_recovery(RecoveryRequest {
            intent: RecoveryIntent::Resume,
            state: &state,
            artifacts: &[],
            baseline: BaselineObservation::Detached,
        });
        assert_eq!(
            decision,
            RecoveryDecision::Blocked(ProtocolBlock::PreviousRecoveryBlocked)
        );
    }

    #[test]
    fn blocked_decisions_preserve_their_reason_contract() {
        let rollback_only = vec![absent(EntryEvidence::Missing, EntryEvidence::Missing)];
        let mut sticky_forward = recovery_state(&rollback_only);
        sticky_forward
            .apply(ProtocolEvent::Action(PublicationAction::RecoveryDecision(
                RecoveryDirection::Forward,
            )))
            .unwrap();
        assert_eq!(
            recovery_decision(
                &sticky_forward,
                &rollback_only,
                RecoveryIntent::Resume,
                BaselineObservation::Base,
            ),
            RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                "the sticky forward decision no longer has complete evidence",
            ))
        );

        let forward = vec![absent(EntryEvidence::Missing, EntryEvidence::New)];
        let state = recovery_state(&forward);
        assert_eq!(
            recovery_decision(
                &state,
                &forward,
                RecoveryIntent::Abandon,
                BaselineObservation::NotBase,
            ),
            RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                "explicit abandon requires the workspace base revision",
            ))
        );

        let still_published = vec![existing(
            EntryEvidence::New,
            EntryEvidence::Missing,
            EntryEvidence::Old,
        )];
        let mut abandoned = recovery_state(&still_published);
        abandoned
            .apply(ProtocolEvent::Action(PublicationAction::RecoveryDecision(
                RecoveryDirection::Rollback,
            )))
            .unwrap();
        abandoned
            .apply(ProtocolEvent::Action(PublicationAction::Abandoned))
            .unwrap();
        assert_eq!(
            recovery_decision(
                &abandoned,
                &still_published,
                RecoveryIntent::Resume,
                BaselineObservation::Base,
            ),
            RecoveryDecision::Blocked(ProtocolBlock::InvalidEventSequence(
                "an abandoned transaction is not fully rolled back",
            ))
        );
    }

    #[test]
    fn fresh_recovery_and_live_commit_share_one_forward_program() {
        let artifacts = vec![
            existing(
                EntryEvidence::Old,
                EntryEvidence::New,
                EntryEvidence::Missing,
            ),
            absent(EntryEvidence::Missing, EntryEvidence::New),
        ];
        let state = recovery_state(&artifacts);
        let mut recovery = Vec::new();
        append_recovery_program(
            &state,
            &artifacts,
            RecoveryDirection::Forward,
            true,
            &mut recovery,
        )
        .unwrap();

        let mut expected = vec![PublicationAction::RecoveryDecision(
            RecoveryDirection::Forward,
        )];
        expected.extend(commit_program(&[true, false]));
        assert_eq!(recovery, recorded(expected.iter().copied()));

        let mut detached = Vec::new();
        append_recovery_program(
            &state,
            &artifacts,
            RecoveryDirection::Forward,
            false,
            &mut detached,
        )
        .unwrap();
        let published = expected
            .iter()
            .position(|action| *action == PublicationAction::Published)
            .unwrap();
        assert_eq!(detached, recorded(expected[..=published].iter().copied()));
    }

    #[test]
    fn recovery_records_completion_only_after_durable_intent() {
        let artifacts = vec![existing(
            EntryEvidence::New,
            EntryEvidence::Missing,
            EntryEvidence::Old,
        )];
        let mut state = recovery_state(&artifacts);
        for action in [
            PublicationAction::StagingVerified,
            PublicationAction::Journaled,
            PublicationAction::BackupIntent(0),
            PublicationAction::BackupCaptured(0),
            PublicationAction::PromotionIntent(0),
        ] {
            state.apply(ProtocolEvent::Action(action)).unwrap();
        }

        let mut actions = Vec::new();
        append_recovery_program(
            &state,
            &artifacts,
            RecoveryDirection::Forward,
            false,
            &mut actions,
        )
        .unwrap();
        assert_eq!(
            actions,
            [
                RecoveryStep::Record(PublicationAction::RecoveryDecision(
                    RecoveryDirection::Forward
                )),
                RecoveryStep::Record(PublicationAction::Promoted(0)),
                RecoveryStep::Record(PublicationAction::Published),
            ]
        );

        let mut missing_intent = recovery_state(&artifacts);
        for action in [
            PublicationAction::StagingVerified,
            PublicationAction::Journaled,
            PublicationAction::BackupIntent(0),
            PublicationAction::BackupCaptured(0),
        ] {
            missing_intent.apply(ProtocolEvent::Action(action)).unwrap();
        }
        assert!(matches!(
            append_recovery_program(
                &missing_intent,
                &artifacts,
                RecoveryDirection::Forward,
                false,
                &mut Vec::new(),
            ),
            Err(ProtocolPlanError::InvalidState(
                "promoted target has no durable intent"
            ))
        ));
    }

    #[test]
    fn recovery_replays_completed_filesystem_actions_without_new_events() {
        let artifacts = vec![existing(
            EntryEvidence::Old,
            EntryEvidence::New,
            EntryEvidence::Missing,
        )];
        let mut state = recovery_state(&artifacts);
        for action in [
            PublicationAction::StagingVerified,
            PublicationAction::Journaled,
            PublicationAction::BackupIntent(0),
            PublicationAction::BackupCaptured(0),
            PublicationAction::PromotionIntent(0),
            PublicationAction::Promoted(0),
        ] {
            state.apply(ProtocolEvent::Action(action)).unwrap();
        }

        let mut steps = Vec::new();
        append_recovery_program(
            &state,
            &artifacts,
            RecoveryDirection::Forward,
            false,
            &mut steps,
        )
        .unwrap();

        assert_eq!(
            steps,
            [
                RecoveryStep::Record(PublicationAction::RecoveryDecision(
                    RecoveryDirection::Forward
                )),
                RecoveryStep::Replay(PublicationAction::BackupCaptured(0)),
                RecoveryStep::Replay(PublicationAction::Promoted(0)),
                RecoveryStep::Record(PublicationAction::Published),
            ]
        );
    }

    #[test]
    fn recovery_reapplies_security_metadata_after_recorded_backup_capture() {
        let artifacts = vec![existing(
            EntryEvidence::Missing,
            EntryEvidence::New,
            EntryEvidence::Old,
        )];
        let mut state = recovery_state(&artifacts);
        for action in [
            PublicationAction::StagingVerified,
            PublicationAction::Journaled,
            PublicationAction::BackupIntent(0),
            PublicationAction::BackupCaptured(0),
        ] {
            state.apply(ProtocolEvent::Action(action)).unwrap();
        }

        let mut steps = Vec::new();
        append_recovery_program(
            &state,
            &artifacts,
            RecoveryDirection::Forward,
            false,
            &mut steps,
        )
        .unwrap();

        assert_eq!(
            steps,
            [
                RecoveryStep::Record(PublicationAction::RecoveryDecision(
                    RecoveryDirection::Forward
                )),
                RecoveryStep::Replay(PublicationAction::BackupCaptured(0)),
                RecoveryStep::Record(PublicationAction::PromotionIntent(0)),
                RecoveryStep::Record(PublicationAction::Promoted(0)),
                RecoveryStep::Record(PublicationAction::Published),
            ]
        );
    }

    #[test]
    fn rollback_program_is_sticky_and_contains_no_forward_action() {
        let artifacts = vec![existing(
            EntryEvidence::Old,
            EntryEvidence::New,
            EntryEvidence::Missing,
        )];
        let state = recovery_state(&artifacts);
        let mut actions = Vec::new();
        append_recovery_program(
            &state,
            &artifacts,
            RecoveryDirection::Rollback,
            true,
            &mut actions,
        )
        .unwrap();
        assert_eq!(
            actions,
            [
                RecoveryStep::Record(PublicationAction::RecoveryDecision(
                    RecoveryDirection::Rollback
                )),
                RecoveryStep::Record(PublicationAction::Abandoned),
                RecoveryStep::Record(PublicationAction::Finalized),
            ]
        );
        assert!(
            !actions
                .iter()
                .copied()
                .map(RecoveryStep::action)
                .any(PublicationAction::is_forward_progress)
        );
    }

    #[test]
    fn finalized_program_redelivers_without_new_durable_actions() {
        let artifacts = vec![absent(EntryEvidence::New, EntryEvidence::Missing)];
        let mut state = recovery_state(&artifacts);
        for action in commit_program(&[false]) {
            state.apply(ProtocolEvent::Action(action)).unwrap();
        }
        let mut actions = Vec::new();
        append_recovery_program(
            &state,
            &artifacts,
            RecoveryDirection::Forward,
            true,
            &mut actions,
        )
        .unwrap();
        assert!(actions.is_empty());
    }
}
