use std::io::{self, Write};
use std::path::Path;
#[cfg(all(test, feature = "decode"))]
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

use serde::Serialize;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, SourceLocator, WorkspaceId, vec_allocation_bytes,
};

use super::CheckedByteCounter;
use super::artifact::{
    EvidenceReadBudget, ExtractionOutputErrorKind, OutputArtifactError, OutputLayout, StagedOutput,
};
use super::contract::{
    ExtractionAllocationUnit, ExtractionArtifactKind, ExtractionDiagnostic,
    ExtractionDiagnosticCode, ExtractionPath,
};
use super::manifest::{
    ExtractionArtifactStatus, ExtractionCanonicalError, ExtractionManifest,
    ExtractionManifestArtifact, ExtractionManifestError, ExtractionReport,
    maximum_extraction_report,
};
use super::model::{ExtractionPlan, PlannedArtifact};
use super::planning_contract::ExtractionPlanError;
use super::publication::{
    ArtifactPublication, ExtractionPublication, PUBLICATION_JOURNAL_PATH, PublicationParameters,
    RECEIPT_SEGMENT_DIRECTORY, receipt_segment_paths,
};
use super::representation::{
    ExtractionReservationError, PreparedRepresentation, RepresentationPreparationError,
    RepresentationRuntime, RepresentationRuntimeContext, RepresentationWriteError,
};
use super::selection::ExtractionPlanner;
#[cfg(feature = "decode")]
use crate::reference::{ReferenceAllocationUnit, ReferenceGraphError};
use crate::workspace::{WorkspaceError, WorkspaceLookup, WorkspaceView};

/// Policy applied when a non-resumable output already occupies a planned path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingOutputPolicy {
    Error,
    Skip,
    Replace,
}

/// Policy applied after one artifact fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionFailurePolicy {
    CollectAll,
    StopInPlanOrder,
}

/// Independent bounds for one extraction execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractionExecutionLimits {
    workers: usize,
    max_in_flight_bytes: u64,
    max_open_files: usize,
    max_output_bytes: u64,
    max_evidence_verification_bytes: u64,
    max_report_bytes: u64,
}

// The extraction lock remains open for the complete run. A worker can briefly hold both a
// parent-directory handle and its staging file, while verified publication can hold the lock,
// two parent directories, the staged file, and a duplicate digest reader.
const EXECUTION_LOCK_OPEN_FILES: usize = 1;
const OPEN_FILES_PER_WORKER: usize = 2;
const SERIAL_OPEN_FILE_PEAK: usize = 5;

impl ExtractionExecutionLimits {
    /// Minimum open-file allowance required for the extraction lock, staging,
    /// digest verification, and safe publication path.
    pub const MIN_OPEN_FILES: usize = SERIAL_OPEN_FILE_PEAK;

    /// Creates validated execution limits.
    ///
    /// `max_open_files` must be at least [`Self::MIN_OPEN_FILES`]. Other
    /// limits and the worker count must be nonzero.
    pub fn new(
        workers: usize,
        max_in_flight_bytes: u64,
        max_open_files: usize,
        max_output_bytes: u64,
        max_evidence_verification_bytes: u64,
        max_report_bytes: u64,
    ) -> Result<Self, ExtractionExecutionError> {
        let limits = Self {
            workers,
            max_in_flight_bytes,
            max_open_files,
            max_output_bytes,
            max_evidence_verification_bytes,
            max_report_bytes,
        };
        limits.validate()?;
        Ok(limits)
    }

    #[must_use]
    pub const fn workers(self) -> usize {
        self.workers
    }

    #[must_use]
    pub const fn max_in_flight_bytes(self) -> u64 {
        self.max_in_flight_bytes
    }

    #[must_use]
    pub const fn max_open_files(self) -> usize {
        self.max_open_files
    }

    #[must_use]
    /// Maximum total bytes published by one run.
    pub const fn max_output_bytes(self) -> u64 {
        self.max_output_bytes
    }

    #[must_use]
    /// Maximum cumulative bytes read from final paths to verify persisted evidence.
    ///
    /// Staging and atomic-publication integrity passes are bounded by
    /// [`Self::max_output_bytes`] instead.
    pub const fn max_evidence_verification_bytes(self) -> u64 {
        self.max_evidence_verification_bytes
    }

    #[must_use]
    pub const fn max_report_bytes(self) -> u64 {
        self.max_report_bytes
    }

    fn validate(self) -> Result<(), ExtractionExecutionError> {
        for (resource, value) in [
            ("workers", u64::try_from(self.workers).unwrap_or(u64::MAX)),
            (
                "open_files",
                u64::try_from(self.max_open_files).unwrap_or(u64::MAX),
            ),
            ("in_flight_bytes", self.max_in_flight_bytes),
            ("output_bytes", self.max_output_bytes),
            (
                "evidence_verification_bytes",
                self.max_evidence_verification_bytes,
            ),
            ("report_bytes", self.max_report_bytes),
        ] {
            if value == 0 {
                return Err(ExtractionExecutionError::InvalidLimit { resource });
            }
        }
        if self.max_open_files < Self::MIN_OPEN_FILES {
            return Err(ExtractionExecutionError::OpenFileLimitTooSmall {
                minimum: Self::MIN_OPEN_FILES,
                limit: self.max_open_files,
            });
        }
        Ok(())
    }

    fn worker_file_capacity(self) -> usize {
        self.max_open_files
            .saturating_sub(EXECUTION_LOCK_OPEN_FILES)
            / OPEN_FILES_PER_WORKER
    }
}

impl Default for ExtractionExecutionLimits {
    fn default() -> Self {
        Self {
            workers: 1,
            max_in_flight_bytes: 512 * 1024 * 1024,
            max_open_files: 32,
            max_output_bytes: 16 * 1024 * 1024 * 1024,
            max_evidence_verification_bytes: 33 * 1024 * 1024 * 1024,
            max_report_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Runtime-only execution choices.
///
/// These fields do not enter the extraction plan or report contracts. The
/// publication journal separately binds the choices that affect recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractionExecutionOptions {
    limits: ExtractionExecutionLimits,
    existing_output: ExistingOutputPolicy,
    failure: ExtractionFailurePolicy,
}

impl ExtractionExecutionOptions {
    pub fn new(
        limits: ExtractionExecutionLimits,
        existing_output: ExistingOutputPolicy,
        failure: ExtractionFailurePolicy,
    ) -> Result<Self, ExtractionExecutionError> {
        limits.validate()?;
        Ok(Self {
            limits,
            existing_output,
            failure,
        })
    }

    #[must_use]
    pub const fn limits(self) -> ExtractionExecutionLimits {
        self.limits
    }

    #[must_use]
    pub const fn existing_output(self) -> ExistingOutputPolicy {
        self.existing_output
    }

    #[must_use]
    pub const fn failure(self) -> ExtractionFailurePolicy {
        self.failure
    }
}

impl Default for ExtractionExecutionOptions {
    fn default() -> Self {
        Self {
            limits: ExtractionExecutionLimits::default(),
            existing_output: ExistingOutputPolicy::Error,
            failure: ExtractionFailurePolicy::CollectAll,
        }
    }
}

/// Ephemeral inputs for one execution of an immutable extraction plan.
#[derive(Debug, Clone, Copy)]
pub struct ExtractionRunOptions<'a> {
    execution: ExtractionExecutionOptions,
    resume: Option<&'a ExtractionManifest>,
    manifest_path: Option<&'a ExtractionPath>,
}

impl<'a> ExtractionRunOptions<'a> {
    #[must_use]
    pub const fn new(execution: ExtractionExecutionOptions) -> Self {
        Self {
            execution,
            resume: None,
            manifest_path: None,
        }
    }
    /// Use a prior manifest only as verification evidence for matching outputs.
    #[must_use]
    pub const fn with_resume(mut self, resume: &'a ExtractionManifest) -> Self {
        self.resume = Some(resume);
        self
    }

    /// Publish the canonical manifest at a validated path under the output root.
    #[must_use]
    pub const fn with_manifest_path(mut self, path: &'a ExtractionPath) -> Self {
        self.manifest_path = Some(path);
        self
    }
}

#[derive(Debug, Default, Clone)]
#[cfg_attr(not(test), derive(Copy))]
struct ExecutionObserver {
    #[cfg(all(test, feature = "decode"))]
    probe: Option<Arc<super::test_probe::ExecutionProbe>>,
}

impl ExecutionObserver {
    const fn none() -> Self {
        Self {
            #[cfg(all(test, feature = "decode"))]
            probe: None,
        }
    }

    #[cfg(all(test, feature = "decode"))]
    fn with_probe(probe: Arc<super::test_probe::ExecutionProbe>) -> Self {
        Self { probe: Some(probe) }
    }

    #[cfg(all(test, feature = "decode"))]
    fn probe(&self) -> Option<&Arc<super::test_probe::ExecutionProbe>> {
        self.probe.as_ref()
    }

    #[cfg(all(test, feature = "decode"))]
    fn reserve_open_files(&self, count: usize) -> super::test_probe::OpenFileGuard {
        super::test_probe::reserve_open_files(self.probe(), count)
    }

    #[cfg(all(test, feature = "decode"))]
    fn enter_work(&self, ordinal: u32, working_set_bytes: u64) -> super::test_probe::WorkGuard {
        super::test_probe::enter_work(self.probe(), ordinal, working_set_bytes)
    }

    #[cfg(all(test, feature = "decode"))]
    fn record_preflight_hash(&self, bytes: u64) {
        super::test_probe::record_preflight_hash(self.probe(), bytes);
    }

    fn before_publication_commit(&self) {
        #[cfg(all(test, feature = "decode"))]
        super::test_probe::before_publication_commit(self.probe());
    }

    #[cfg(all(test, feature = "decode"))]
    fn observe_writer<'writer>(
        &self,
        ordinal: u32,
        writer: &'writer mut dyn Write,
    ) -> super::test_probe::ObservedWriter<'writer> {
        super::test_probe::ObservedWriter::new(self.probe(), ordinal, writer)
    }
}

/// Executes immutable extraction plans against their exact workspace revision.
#[derive(Debug, Default, Clone)]
#[cfg_attr(not(test), derive(Copy))]
pub struct ExtractionExecutor {
    observer: ExecutionObserver,
}

struct ExecutionInvocation<'a> {
    view: &'a dyn WorkspaceView,
    plan: &'a ExtractionPlan,
    output_root: &'a Path,
    manifest_path: Option<&'a ExtractionPath>,
    options: &'a ExtractionExecutionOptions,
    resume: Option<&'a ExtractionManifest>,
}

impl ExtractionExecutor {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            observer: ExecutionObserver::none(),
        }
    }

    /// Reads the workspace identity bound to an existing publication journal.
    ///
    /// Process frontends should use this read-only hint before reconstructing a
    /// workspace for request-based recovery. The executor still verifies the
    /// complete journal, plan, source, and revision identity under its output
    /// lock before resuming publication.
    pub fn publication_workspace_id(
        output_root: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<WorkspaceId>, ExtractionExecutionError> {
        super::publication::publication_workspace_id(output_root, budget)
    }

    #[cfg(all(test, feature = "decode"))]
    fn observing(probe: Arc<super::test_probe::ExecutionProbe>) -> Self {
        Self {
            observer: ExecutionObserver::with_probe(probe),
        }
    }

    /// Execute an immutable plan against the exact workspace revision.
    ///
    /// A resume manifest in `run` is verification evidence, not overwrite authority.
    /// Missing or mismatched outputs continue to obey the selected existing-output policy.
    pub fn execute(
        &self,
        view: &dyn WorkspaceView,
        plan: &ExtractionPlan,
        output_root: &Path,
        run: ExtractionRunOptions<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionReport, ExtractionExecutionError> {
        let ExtractionRunOptions {
            execution,
            resume,
            manifest_path,
        } = run;
        self.execute_inner(
            ExecutionInvocation {
                view,
                plan,
                output_root,
                manifest_path,
                options: &execution,
                resume,
            },
            budget,
        )
    }

    fn execute_inner(
        &self,
        invocation: ExecutionInvocation<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionReport, ExtractionExecutionError> {
        let ExecutionInvocation {
            view,
            plan,
            output_root,
            manifest_path,
            options,
            resume,
        } = invocation;
        options.limits.validate()?;
        validate_resume(plan, resume)?;
        let manifest_output_reservation =
            validate_contract_bounds(plan, options.limits, manifest_path.is_some())?;
        let artifact_output_limit = options
            .limits
            .max_output_bytes
            .checked_sub(manifest_output_reservation)
            .expect("validated manifest reservation exceeds the output limit");
        let parameters = PublicationParameters::new(
            *options,
            artifact_output_limit,
            manifest_output_reservation,
            manifest_path,
            resume,
        );
        let journal_exists = OutputLayout::has_existing(output_root, PUBLICATION_JOURNAL_PATH)
            .map_err(ExtractionExecutionError::output_layout)?;
        let mut verified_plan = None;
        #[cfg(feature = "decode")]
        let preflight_working_sets = None;
        #[cfg(not(feature = "decode"))]
        let mut preflight_working_sets = None;
        if !journal_exists {
            validate_context(view, plan)?;
            validate_sources(view, plan, budget)?;
            let verified = ExtractionPlanner::new(view).verify(plan, budget)?;
            #[cfg(feature = "decode")]
            validate_planned_working_set_limits(&verified, options.limits)?;
            #[cfg(not(feature = "decode"))]
            {
                let representation_context = RepresentationRuntimeContext::load(
                    view,
                    verified
                        .artifacts()
                        .iter()
                        .map(|artifact| artifact.representation()),
                    budget,
                )?;
                let representation_runtime = representation_context.bind(view, budget)?;
                preflight_working_sets = Some(prove_working_sets(
                    &representation_runtime,
                    &verified,
                    options.limits,
                    budget,
                )?);
            }
            verified_plan = Some(verified);
        }

        let receipt_segment_paths = receipt_segment_paths(plan.artifacts().len(), budget)?;
        let relative_paths = plan
            .artifacts()
            .iter()
            .flat_map(|artifact| {
                std::iter::once(artifact.preferred_path().as_str())
                    .chain(artifact.fallback_path().map(ExtractionPath::as_str))
            })
            .chain(manifest_path.into_iter().map(ExtractionPath::as_str));
        let internal_paths = std::iter::once(PUBLICATION_JOURNAL_PATH)
            .chain(receipt_segment_paths.iter().map(String::as_str));
        let layout = OutputLayout::prepare_with_internal_paths(
            output_root,
            relative_paths,
            internal_paths,
            &[RECEIPT_SEGMENT_DIRECTORY],
        )
        .map_err(ExtractionExecutionError::output_layout)?;
        #[cfg(all(test, feature = "decode"))]
        let _execution_open_file = self.observer.reserve_open_files(EXECUTION_LOCK_OPEN_FILES);

        let mut publication =
            ExtractionPublication::open(&layout, &receipt_segment_paths, plan, parameters, budget)?;
        if publication
            .as_ref()
            .is_some_and(|publication| publication.completed_artifacts() == plan.artifacts().len())
        {
            return finalize_publication(
                &layout,
                publication
                    .take()
                    .expect("completed publication was just observed"),
                manifest_path,
                manifest_output_reservation,
                options.limits.max_report_bytes,
                &self.observer,
            );
        }
        if journal_exists {
            validate_context(view, plan)?;
            validate_sources(view, plan, budget)?;
            verified_plan = Some(ExtractionPlanner::new(view).verify(plan, budget)?);
        }
        let verified_plan = verified_plan.expect("one verification branch always runs");
        let plan = &*verified_plan;
        let representation_context = RepresentationRuntimeContext::load(
            view,
            plan.artifacts()
                .iter()
                .map(|artifact| artifact.representation()),
            budget,
        )?;
        let representation_runtime = representation_context.bind(view, budget)?;
        let working_sets = match preflight_working_sets {
            Some(working_sets) => working_sets,
            None => prove_working_sets(&representation_runtime, plan, options.limits, budget)?,
        };
        let mut publication = match publication {
            Some(publication) => publication,
            None => ExtractionPublication::create(
                &layout,
                &receipt_segment_paths,
                plan,
                parameters,
                budget,
            )?,
        };
        let completed_artifacts = publication.completed_artifacts();
        let mut outcomes = (0..plan.artifacts().len())
            .map(|_| None)
            .collect::<Vec<Option<WorkOutcome>>>();
        let mut pending = Vec::new();
        let mut preflight_stopped = publication.stopped();
        let evidence_read_budget = publication.evidence_read_budget_mut();
        for (index, artifact) in plan
            .artifacts()
            .iter()
            .enumerate()
            .skip(completed_artifacts)
        {
            if preflight_stopped {
                continue;
            }
            match resumed_artifact(
                &layout,
                artifact,
                resume,
                evidence_read_budget,
                &self.observer,
            )? {
                ResumeDecision::Complete(receipt) => {
                    outcomes[index] = Some(WorkOutcome::Receipt(receipt));
                }
                ResumeDecision::Execute(resume_evidence) => {
                    let preferred_target = prepare_target(
                        &layout,
                        artifact.preferred_path(),
                        options.existing_output,
                        resume_evidence_for_slot(resume_evidence, PlannedOutputSlot::Preferred),
                        evidence_read_budget,
                        &self.observer,
                    )?;
                    if let Some(outcome) = prepared_target_outcome(
                        artifact,
                        artifact.preferred_kind(),
                        artifact.preferred_path(),
                        artifact.diagnostics().to_vec(),
                        preferred_target,
                    ) {
                        preflight_stopped = options.failure
                            == ExtractionFailurePolicy::StopInPlanOrder
                            && matches!(
                                &outcome,
                                WorkOutcome::Receipt(receipt)
                                    if receipt.status() == ExtractionArtifactStatus::Failed
                            );
                        outcomes[index] = Some(outcome);
                        continue;
                    }
                    pending.push(PendingWork {
                        artifact_index: index,
                        working_set_bytes: working_sets[index],
                        preferred_target,
                        fallback_evidence: resume_evidence_for_slot(
                            resume_evidence,
                            PlannedOutputSlot::Fallback,
                        ),
                    });
                }
            }
        }

        let mut pending_cursor = 0;
        let mut publish_cursor = completed_artifacts;
        while pending_cursor < pending.len() && !publication.stopped() {
            let batch = PendingBatch::select(options.limits, &pending, pending_cursor)?;
            let batch_end = batch.end;
            debug_assert!(batch.working_set_bytes <= options.limits.max_in_flight_bytes);
            debug_assert!(batch.open_files <= options.limits.max_open_files);
            let remaining_output = publication.remaining_output();
            let results = execute_pending_batch(
                plan,
                &layout,
                options,
                budget,
                &representation_runtime,
                &pending[pending_cursor..batch_end],
                remaining_output,
                &self.observer,
            );
            for (work, outcome) in pending[pending_cursor..batch_end].iter().zip(results) {
                outcomes[work.artifact_index] = Some(resolve_fallback_outcome(
                    &plan.artifacts()[work.artifact_index],
                    work,
                    outcome,
                    FallbackResolution {
                        layout: &layout,
                        existing: options.existing_output,
                        output_limit: remaining_output.min(work.working_set_bytes),
                        evidence_read_budget: publication.evidence_read_budget_mut(),
                        observer: &self.observer,
                    },
                ));
            }
            let ready_end = pending[batch_end - 1].artifact_index + 1;
            publish_ready(
                plan,
                options,
                &mut outcomes[publish_cursor..ready_end],
                publish_cursor,
                &mut publication,
                &self.observer,
            )?;
            publish_cursor = ready_end;
            pending_cursor = batch_end;
        }

        publish_ready(
            plan,
            options,
            &mut outcomes[publish_cursor..],
            publish_cursor,
            &mut publication,
            &self.observer,
        )?;
        finalize_publication(
            &layout,
            publication,
            manifest_path,
            manifest_output_reservation,
            options.limits.max_report_bytes,
            &self.observer,
        )
    }
}

fn finalize_publication(
    layout: &OutputLayout,
    publication: ExtractionPublication<'_, '_>,
    manifest_path: Option<&ExtractionPath>,
    manifest_output_limit: u64,
    report_limit: u64,
    observer: &ExecutionObserver,
) -> Result<ExtractionReport, ExtractionExecutionError> {
    let mut publication = publication.finish()?;
    validate_actual_report(publication.report(), report_limit)?;
    if let Some(manifest_path) = manifest_path
        && publication.needs_manifest_publication()
    {
        let staged = stage_manifest(
            layout,
            manifest_path,
            publication.report(),
            manifest_output_limit,
            observer,
        )?;
        let publish_result = {
            #[cfg(all(test, feature = "decode"))]
            let _publish_open_files =
                observer.reserve_open_files(SERIAL_OPEN_FILE_PEAK - EXECUTION_LOCK_OPEN_FILES);
            publication.publish_manifest(staged)
        };
        publish_result?;
    }
    observer.before_publication_commit();
    publication.commit()
}

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ExtractionExecutionError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Canonical(#[from] ExtractionCanonicalError),
    #[error(transparent)]
    Manifest(#[from] ExtractionManifestError),
    #[error(transparent)]
    PlanVerification(Box<ExtractionPlanError>),
    #[error("extraction limit {resource} must be nonzero")]
    InvalidLimit { resource: &'static str },
    #[error("open-file limit {limit} is below the safe extraction minimum {minimum}")]
    OpenFileLimitTooSmall { minimum: usize, limit: usize },
    #[error("extraction plan belongs to a different workspace revision")]
    WorkspaceContextMismatch,
    #[error("extraction source changed or is unavailable: {locator:?}")]
    SourceChanged { locator: SourceLocator },
    #[error("artifact {ordinal} requires {required} in-flight bytes, exceeding the limit {limit}")]
    WorkingSetExceedsLimit {
        ordinal: u32,
        required: u64,
        limit: u64,
    },
    #[error(
        "artifact {ordinal} declares {declared} in-flight bytes but requires at least {required}"
    )]
    WorkingSetUnderdeclared {
        ordinal: u32,
        declared: u64,
        required: u64,
    },
    #[error("artifact {ordinal} no longer matches its persisted working-set proof")]
    WorkingSetProofFailed { ordinal: u32 },
    #[error("artifact {ordinal} prepared media descriptor no longer matches its extraction plan")]
    MediaDescriptorChanged { ordinal: u32 },
    #[error("artifact {ordinal} media can no longer be prepared from its validated source")]
    MediaPreparationFailed { ordinal: u32 },
    #[error("canonical extraction report requires at most {required} bytes, limit is {limit}")]
    ReportLimitExceeded { required: u64, limit: u64 },
    #[error(
        "canonical extraction manifest requires at most {required} output bytes, limit is {limit}"
    )]
    ManifestOutputLimitExceeded { required: u64, limit: u64 },
    #[error("failed to prepare the safe output layout during {kind:?}: {message}")]
    OutputLayout {
        kind: ExtractionOutputErrorKind,
        message: String,
    },
    #[error("resume manifest does not describe this exact extraction plan")]
    ResumePlanMismatch,
    #[error("failed to reserve {requested} {unit} for {resource}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        unit: ExtractionAllocationUnit,
    },
    #[error("an extraction worker panicked while processing ordinal {ordinal}")]
    WorkerPanicked { ordinal: u32 },
    #[error("an extraction worker did not return an outcome for ordinal {ordinal}")]
    MissingWorkerOutcome { ordinal: u32 },
    #[error("failed to serialize the bounded report: {0}")]
    ReportSerialization(String),
    #[error("extraction output byte accounting overflowed")]
    OutputLengthOverflow,
    #[error("invalid extraction publication journal: {message}")]
    PublicationJournalInvalid { message: String },
    #[error("canonical extraction publication journal requires {required} bytes, limit is {limit}")]
    PublicationJournalLimitExceeded { required: u64, limit: u64 },
    #[error("evidence verification requires {required} bytes, only {remaining} remain")]
    EvidenceVerificationLimitExceeded { required: u64, remaining: u64 },
    #[error("extraction publication journal conflict: {reason}")]
    PublicationJournalConflict { reason: &'static str },
    #[error("extraction publication requires recovery at stage {stage}")]
    PublicationRecoveryRequired { stage: &'static str },
}

impl ExtractionExecutionError {
    pub(super) fn output_layout(error: OutputArtifactError) -> Self {
        Self::OutputLayout {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl From<ExtractionPlanError> for ExtractionExecutionError {
    fn from(error: ExtractionPlanError) -> Self {
        Self::PlanVerification(Box::new(error))
    }
}

#[derive(Debug)]
struct PendingWork {
    artifact_index: usize,
    working_set_bytes: u64,
    preferred_target: PreparedTarget,
    fallback_evidence: Option<ExistingEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingBatch {
    end: usize,
    working_set_bytes: u64,
    open_files: usize,
}

impl PendingBatch {
    fn select(
        limits: ExtractionExecutionLimits,
        pending: &[PendingWork],
        start: usize,
    ) -> Result<Self, ExtractionExecutionError> {
        let maximum_count = limits.workers.min(limits.worker_file_capacity());
        let mut end = start;
        let mut working_set_bytes = 0_u64;
        while let Some(work) = pending.get(end) {
            if end - start == maximum_count {
                break;
            }
            let Some(next) = working_set_bytes.checked_add(work.working_set_bytes) else {
                return Err(ExtractionExecutionError::OutputLengthOverflow);
            };
            if end > start && next > limits.max_in_flight_bytes {
                break;
            }
            working_set_bytes = next;
            end += 1;
        }
        let worker_count = end - start;
        let open_files = SERIAL_OPEN_FILE_PEAK
            .max(EXECUTION_LOCK_OPEN_FILES + worker_count * OPEN_FILES_PER_WORKER);
        Ok(Self {
            end,
            working_set_bytes,
            open_files,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum PreparedTarget {
    Encode { replace: bool },
    Existing { length: u64, digest: DigestV1 },
    Failed(ExtractionDiagnosticCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannedOutputSlot {
    Preferred,
    Fallback,
}

#[derive(Debug, Clone, Copy)]
enum ExistingEvidence {
    Missing,
    Existing { length: u64, digest: DigestV1 },
    HashLimitExceeded { required: u64, remaining: u64 },
}

#[derive(Debug, Clone, Copy)]
struct ResumeEvidence {
    slot: PlannedOutputSlot,
    existing: ExistingEvidence,
}

fn resume_evidence_for_slot(
    evidence: Option<ResumeEvidence>,
    slot: PlannedOutputSlot,
) -> Option<ExistingEvidence> {
    evidence
        .filter(|evidence| evidence.slot == slot)
        .map(|evidence| evidence.existing)
}

enum ResumeDecision {
    Complete(ExtractionManifestArtifact),
    Execute(Option<ResumeEvidence>),
}

fn validate_context(
    view: &dyn WorkspaceView,
    plan: &ExtractionPlan,
) -> Result<(), ExtractionExecutionError> {
    if view.workspace_id() != plan.workspace_id() || view.revision() != plan.revision() {
        return Err(ExtractionExecutionError::WorkspaceContextMismatch);
    }
    Ok(())
}

fn validate_sources(
    view: &dyn WorkspaceView,
    plan: &ExtractionPlan,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionExecutionError> {
    for expected in plan.sources() {
        let source = match view.resolve_source(expected.locator(), budget)? {
            WorkspaceLookup::Resolved(source) => source,
            WorkspaceLookup::Unloaded
            | WorkspaceLookup::Missing
            | WorkspaceLookup::Ambiguous { .. }
            | WorkspaceLookup::Invalid { .. } => {
                return Err(ExtractionExecutionError::SourceChanged {
                    locator: expected.locator().clone(),
                });
            }
        };
        if source.fingerprint() != expected.fingerprint() {
            return Err(ExtractionExecutionError::SourceChanged {
                locator: expected.locator().clone(),
            });
        }
    }
    Ok(())
}

fn validate_resume(
    plan: &ExtractionPlan,
    resume: Option<&ExtractionManifest>,
) -> Result<(), ExtractionExecutionError> {
    let Some(resume) = resume else {
        return Ok(());
    };
    if resume.workspace_id() != plan.workspace_id()
        || resume.revision() != plan.revision()
        || resume.request_digest() != plan.request_digest()
        || resume.plan_digest() != plan.digest()?
        || resume.sources() != plan.sources()
    {
        return Err(ExtractionExecutionError::ResumePlanMismatch);
    }
    Ok(())
}

fn prove_working_sets(
    runtime: &RepresentationRuntime<'_, '_>,
    plan: &ExtractionPlan,
    limits: ExtractionExecutionLimits,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u64>, ExtractionExecutionError> {
    let count = plan.artifacts().len();
    let entries = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "trusted extraction working sets",
    })?;
    let minimum_bytes =
        vec_allocation_bytes::<u64>(count).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "trusted extraction working sets",
        })?;
    budget.check_entries(entries)?;
    budget.check_bytes(minimum_bytes)?;

    let mut working_sets = Vec::new();
    working_sets
        .try_reserve_exact(count)
        .map_err(|_| ExtractionExecutionError::Allocation {
            resource: "trusted extraction working sets",
            requested: count,
            unit: ExtractionAllocationUnit::CapacityUnits,
        })?;
    let retained_bytes = vec_allocation_bytes::<u64>(working_sets.capacity()).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "trusted extraction working sets",
        }
    })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(retained_bytes)?;
    for artifact in plan.artifacts() {
        let required =
            runtime.trusted_working_set(artifact.address(), artifact.representation(), budget);
        let required =
            required.map_err(|error| map_reservation_error(artifact.ordinal(), error))?;
        if artifact.working_set_bytes() < required {
            return Err(ExtractionExecutionError::WorkingSetUnderdeclared {
                ordinal: artifact.ordinal(),
                declared: artifact.working_set_bytes(),
                required,
            });
        }
        if required > limits.max_in_flight_bytes {
            return Err(ExtractionExecutionError::WorkingSetExceedsLimit {
                ordinal: artifact.ordinal(),
                required,
                limit: limits.max_in_flight_bytes,
            });
        }
        working_sets.push(required);
    }
    Ok(working_sets)
}

#[cfg(feature = "decode")]
fn validate_planned_working_set_limits(
    plan: &ExtractionPlan,
    limits: ExtractionExecutionLimits,
) -> Result<(), ExtractionExecutionError> {
    for artifact in plan.artifacts() {
        let required = artifact.working_set_bytes();
        if required > limits.max_in_flight_bytes {
            return Err(ExtractionExecutionError::WorkingSetExceedsLimit {
                ordinal: artifact.ordinal(),
                required,
                limit: limits.max_in_flight_bytes,
            });
        }
    }
    Ok(())
}

#[cfg(feature = "decode")]
fn map_reference_graph_error(ordinal: u32, error: ReferenceGraphError) -> ExtractionExecutionError {
    match error {
        ReferenceGraphError::Budget(error)
        | ReferenceGraphError::Workspace(WorkspaceError::Budget(error)) => error.into(),
        ReferenceGraphError::Workspace(error) => error.into(),
        ReferenceGraphError::Allocation {
            resource,
            requested,
            unit,
            ..
        } => ExtractionExecutionError::Allocation {
            resource,
            requested,
            unit: match unit {
                ReferenceAllocationUnit::Bytes => ExtractionAllocationUnit::Bytes,
                ReferenceAllocationUnit::Elements => ExtractionAllocationUnit::CapacityUnits,
            },
        },
        ReferenceGraphError::Contract(_)
        | ReferenceGraphError::Binary(_)
        | ReferenceGraphError::Diagnostic(_)
        | ReferenceGraphError::FieldPath(_)
        | ReferenceGraphError::Yaml(_)
        | ReferenceGraphError::CachePoisoned
        | ReferenceGraphError::ReferenceSourceKindMismatch { .. }
        | ReferenceGraphError::Invariant(_)
        | ReferenceGraphError::ObjectNotIndexed
        | ReferenceGraphError::ProjectionIo { .. }
        | ReferenceGraphError::ProjectionEncoding { .. } => {
            ExtractionExecutionError::WorkingSetProofFailed { ordinal }
        }
    }
}

fn map_reservation_error(
    ordinal: u32,
    error: ExtractionReservationError,
) -> ExtractionExecutionError {
    match error {
        ExtractionReservationError::Workspace(error) => error.into(),
        ExtractionReservationError::Budget(error) => error.into(),
        #[cfg(feature = "decode")]
        ExtractionReservationError::Reference(error) => map_reference_graph_error(ordinal, error),
        ExtractionReservationError::ArithmeticOverflow { .. } => {
            ExtractionExecutionError::OutputLengthOverflow
        }
        ExtractionReservationError::ObjectUnavailable(_)
        | ExtractionReservationError::ContentMismatch(_)
        | ExtractionReservationError::YamlSizing(_) => {
            ExtractionExecutionError::WorkingSetProofFailed { ordinal }
        }
    }
}

fn validate_contract_bounds(
    plan: &ExtractionPlan,
    limits: ExtractionExecutionLimits,
    reserve_manifest_output: bool,
) -> Result<u64, ExtractionExecutionError> {
    let bound = maximum_extraction_report(plan)?;
    let required = canonical_length(&bound)?;
    if required > limits.max_report_bytes {
        return Err(ExtractionExecutionError::ReportLimitExceeded {
            required,
            limit: limits.max_report_bytes,
        });
    }
    if !reserve_manifest_output {
        return Ok(0);
    }
    let required = canonical_length(&bound.manifest())?;
    if required > limits.max_output_bytes {
        return Err(ExtractionExecutionError::ManifestOutputLimitExceeded {
            required,
            limit: limits.max_output_bytes,
        });
    }
    Ok(required)
}

fn stage_manifest(
    layout: &OutputLayout,
    path: &ExtractionPath,
    report: &ExtractionReport,
    output_limit: u64,
    _observer: &ExecutionObserver,
) -> Result<StagedOutput, ExtractionExecutionError> {
    let required = canonical_length(report.manifest())?;
    if required > output_limit {
        return Err(ExtractionExecutionError::ManifestOutputLimitExceeded {
            required,
            limit: output_limit,
        });
    }
    let output = layout
        .path(path.as_str())
        .map_err(ExtractionExecutionError::output_layout)?;
    let staged = {
        #[cfg(all(test, feature = "decode"))]
        let _stage_open_files = _observer.reserve_open_files(OPEN_FILES_PER_WORKER);
        let mut staging = output
            .create_staging()
            .map_err(ExtractionExecutionError::output_layout)?;
        report.write_canonical_manifest_json(staging.writer())?;
        staging
            .finish()
            .map_err(ExtractionExecutionError::output_layout)?
    };
    Ok(staged)
}

fn resumed_artifact(
    layout: &OutputLayout,
    artifact: &PlannedArtifact,
    resume: Option<&ExtractionManifest>,
    evidence_read_budget: &mut EvidenceReadBudget,
    observer: &ExecutionObserver,
) -> Result<ResumeDecision, ExtractionExecutionError> {
    let Some(candidate) =
        resume.and_then(|manifest| manifest.artifact_by_ordinal(artifact.ordinal()))
    else {
        return Ok(ResumeDecision::Execute(None));
    };
    let resumable_status = matches!(
        candidate.status(),
        ExtractionArtifactStatus::Written | ExtractionArtifactStatus::Resumed
    );
    let slot = if !artifact.matches_output(candidate.kind(), candidate.path()) {
        None
    } else if artifact.preferred_kind() == candidate.kind()
        && artifact.preferred_path() == candidate.path()
    {
        Some(PlannedOutputSlot::Preferred)
    } else {
        Some(PlannedOutputSlot::Fallback)
    };
    let evidence = candidate.length().zip(candidate.digest());
    if resumable_status
        && artifact.address() == candidate.address()
        && let Some(slot) = slot
        && let Some((expected_length, expected_digest)) = evidence
    {
        let output = layout
            .path(candidate.path().as_str())
            .map_err(ExtractionExecutionError::output_layout)?;
        match hash_existing_bounded(output, evidence_read_budget, observer) {
            Ok(Some(actual)) if actual == (expected_length, expected_digest) => {
                let diagnostics = artifact_diagnostics_with(
                    artifact,
                    artifact
                        .fallback_path()
                        .filter(|path| *path == candidate.path())
                        .map(|_| runtime_fallback_diagnostic()),
                );
                return Ok(ResumeDecision::Complete(ExtractionManifestArtifact::new(
                    artifact.ordinal(),
                    artifact.address().clone(),
                    candidate.kind(),
                    candidate.path().clone(),
                    ExtractionArtifactStatus::Resumed,
                    Some(expected_length),
                    Some(expected_digest),
                    diagnostics,
                )?));
            }
            Ok(Some((length, digest))) => {
                return Ok(ResumeDecision::Execute(Some(ResumeEvidence {
                    slot,
                    existing: ExistingEvidence::Existing { length, digest },
                })));
            }
            Ok(None) => {
                return Ok(ResumeDecision::Execute(Some(ResumeEvidence {
                    slot,
                    existing: ExistingEvidence::Missing,
                })));
            }
            Err(OutputArtifactError::ExistingHashLimitExceeded { length, limit, .. }) => {
                return Ok(ResumeDecision::Execute(Some(ResumeEvidence {
                    slot,
                    existing: ExistingEvidence::HashLimitExceeded {
                        required: length,
                        remaining: limit,
                    },
                })));
            }
            Err(error) => return Err(ExtractionExecutionError::output_layout(error)),
        }
    }
    Ok(ResumeDecision::Execute(None))
}

const fn runtime_fallback_diagnostic() -> ExtractionDiagnosticCode {
    #[cfg(feature = "decode")]
    {
        ExtractionDiagnosticCode::DecodeFailedRawFallback
    }
    #[cfg(not(feature = "decode"))]
    {
        ExtractionDiagnosticCode::FeatureUnavailable
    }
}

fn prepare_target(
    layout: &OutputLayout,
    path: &ExtractionPath,
    existing: ExistingOutputPolicy,
    evidence: Option<ExistingEvidence>,
    evidence_read_budget: &mut EvidenceReadBudget,
    observer: &ExecutionObserver,
) -> Result<PreparedTarget, ExtractionExecutionError> {
    if let Some(evidence) = evidence {
        return target_from_existing_evidence(existing, evidence);
    }
    let output = match layout.path(path.as_str()) {
        Ok(output) => output,
        Err(_) => {
            return Ok(PreparedTarget::Failed(
                ExtractionDiagnosticCode::OutputFailed,
            ));
        }
    };
    Ok(match existing {
        ExistingOutputPolicy::Error => match output.exists() {
            Ok(true) => PreparedTarget::Failed(ExtractionDiagnosticCode::OutputExists),
            Ok(false) => PreparedTarget::Encode { replace: false },
            Err(_) => PreparedTarget::Failed(ExtractionDiagnosticCode::OutputFailed),
        },
        ExistingOutputPolicy::Replace => PreparedTarget::Encode { replace: true },
        ExistingOutputPolicy::Skip => {
            match hash_existing_bounded(output, evidence_read_budget, observer) {
                Ok(Some((length, digest))) => PreparedTarget::Existing { length, digest },
                Ok(None) => PreparedTarget::Encode { replace: false },
                Err(OutputArtifactError::ExistingHashLimitExceeded { length, limit, .. }) => {
                    return Err(
                        ExtractionExecutionError::EvidenceVerificationLimitExceeded {
                            required: length,
                            remaining: limit,
                        },
                    );
                }
                Err(_) => PreparedTarget::Failed(ExtractionDiagnosticCode::OutputFailed),
            }
        }
    })
}

fn target_from_existing_evidence(
    policy: ExistingOutputPolicy,
    evidence: ExistingEvidence,
) -> Result<PreparedTarget, ExtractionExecutionError> {
    Ok(match evidence {
        ExistingEvidence::Missing => PreparedTarget::Encode { replace: false },
        ExistingEvidence::Existing { length, digest } => match policy {
            ExistingOutputPolicy::Error => {
                PreparedTarget::Failed(ExtractionDiagnosticCode::OutputExists)
            }
            ExistingOutputPolicy::Replace => PreparedTarget::Encode { replace: true },
            ExistingOutputPolicy::Skip => PreparedTarget::Existing { length, digest },
        },
        ExistingEvidence::HashLimitExceeded {
            required,
            remaining,
        } => match policy {
            ExistingOutputPolicy::Error => {
                PreparedTarget::Failed(ExtractionDiagnosticCode::OutputExists)
            }
            ExistingOutputPolicy::Replace => PreparedTarget::Encode { replace: true },
            ExistingOutputPolicy::Skip => {
                return Err(
                    ExtractionExecutionError::EvidenceVerificationLimitExceeded {
                        required,
                        remaining,
                    },
                );
            }
        },
    })
}

fn hash_existing_bounded(
    output: &super::artifact::PreparedOutputPath,
    budget: &mut EvidenceReadBudget,
    _observer: &ExecutionObserver,
) -> Result<Option<(u64, DigestV1)>, OutputArtifactError> {
    let result = output.hash_existing_bounded(budget)?;
    #[cfg(all(test, feature = "decode"))]
    if let Some((length, _)) = result {
        _observer.record_preflight_hash(length);
    }
    Ok(result)
}

fn prepared_target_outcome(
    artifact: &PlannedArtifact,
    kind: ExtractionArtifactKind,
    path: &ExtractionPath,
    diagnostics: Vec<ExtractionDiagnostic>,
    target: PreparedTarget,
) -> Option<WorkOutcome> {
    match target {
        PreparedTarget::Encode { .. } => None,
        PreparedTarget::Existing { length, digest } => Some(receipt_outcome(
            artifact,
            kind,
            path,
            ExtractionArtifactStatus::SkippedExisting,
            Some(length),
            Some(digest),
            diagnostics,
        )),
        PreparedTarget::Failed(code) => {
            let mut diagnostics = diagnostics;
            diagnostics.push(ExtractionDiagnostic::new(
                code,
                Some(artifact.address().clone()),
            ));
            Some(receipt_outcome(
                artifact,
                kind,
                path,
                ExtractionArtifactStatus::Failed,
                None,
                None,
                diagnostics,
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_pending_batch(
    plan: &ExtractionPlan,
    layout: &OutputLayout,
    options: &ExtractionExecutionOptions,
    budget: &mut AssetLoadBudget,
    representation_runtime: &RepresentationRuntime<'_, '_>,
    pending: &[PendingWork],
    output_limit: u64,
    observer: &ExecutionObserver,
) -> Vec<WorkOutcome> {
    let worker_count = options.limits.workers.min(pending.len());
    let next = AtomicUsize::new(0);
    let results = Mutex::new(
        (0..pending.len())
            .map(|_| None)
            .collect::<Vec<Option<WorkOutcome>>>(),
    );
    let budget = Mutex::new(budget);
    let preparation = PreparationOrder::default();

    thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            workers.push(scope.spawn(|| {
                loop {
                    let pending_index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(work) = pending.get(pending_index) else {
                        break;
                    };
                    let artifact = &plan.artifacts()[work.artifact_index];
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        process_artifact(
                            artifact,
                            work,
                            layout,
                            pending_index,
                            &budget,
                            &preparation,
                            representation_runtime,
                            output_limit,
                            observer,
                        )
                    }))
                    .unwrap_or_else(|_| {
                        preparation.skip(pending_index);
                        WorkOutcome::Fatal(ExtractionExecutionError::WorkerPanicked {
                            ordinal: artifact.ordinal(),
                        })
                    });
                    lock_recover(&results)[pending_index] = Some(result);
                }
            }));
        }
        for worker in workers {
            let _ = worker.join();
        }
    });

    let mut results = lock_recover(&results);
    results
        .iter_mut()
        .enumerate()
        .map(|(index, result)| {
            result.take().unwrap_or_else(|| {
                WorkOutcome::Fatal(ExtractionExecutionError::MissingWorkerOutcome {
                    ordinal: plan.artifacts()[pending[index].artifact_index].ordinal(),
                })
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn process_artifact(
    artifact: &PlannedArtifact,
    work: &PendingWork,
    layout: &OutputLayout,
    pending_index: usize,
    budget: &Mutex<&mut AssetLoadBudget>,
    preparation: &PreparationOrder,
    representation_runtime: &RepresentationRuntime<'_, '_>,
    output_limit: u64,
    observer: &ExecutionObserver,
) -> WorkOutcome {
    #[cfg(all(test, feature = "decode"))]
    let _active_work = observer.enter_work(artifact.ordinal(), work.working_set_bytes);
    let prepared = preparation.run(pending_index, || {
        let mut budget = lock_recover(budget);
        let input = representation_runtime
            .prepare(artifact.address(), artifact.representation(), &mut budget)
            .map_err(|error| map_representation_preparation_error(artifact.ordinal(), error))?;
        if artifact.preferred_requires_write_budget() {
            return Ok(PreparedWork::Complete(encode_artifact(
                artifact,
                work,
                input,
                layout,
                output_limit.min(work.working_set_bytes),
                Some(&mut budget),
                observer,
            )));
        }
        Ok(PreparedWork::Input(input))
    });
    match prepared {
        Err(error) => WorkOutcome::Fatal(error),
        Ok(PreparedWork::Complete(outcome)) => outcome,
        Ok(PreparedWork::Input(input)) => encode_artifact(
            artifact,
            work,
            input,
            layout,
            output_limit.min(work.working_set_bytes),
            None,
            observer,
        ),
    }
}

enum PreparedWork {
    Input(PreparedRepresentation),
    Complete(WorkOutcome),
}

fn map_representation_preparation_error(
    ordinal: u32,
    error: RepresentationPreparationError,
) -> ExtractionExecutionError {
    match error {
        RepresentationPreparationError::Workspace(error) => error.into(),
        RepresentationPreparationError::Budget(error) => error.into(),
        #[cfg(feature = "decode")]
        RepresentationPreparationError::Allocation {
            resource,
            requested,
        } => ExtractionExecutionError::Allocation {
            resource,
            requested,
            unit: ExtractionAllocationUnit::Bytes,
        },
        #[cfg(feature = "decode")]
        RepresentationPreparationError::SourceChanged(locator) => {
            ExtractionExecutionError::SourceChanged { locator }
        }
        #[cfg(feature = "decode")]
        RepresentationPreparationError::DescriptorChanged => {
            ExtractionExecutionError::MediaDescriptorChanged { ordinal }
        }
        RepresentationPreparationError::InvalidContent => {
            ExtractionExecutionError::MediaPreparationFailed { ordinal }
        }
    }
}

fn encode_artifact(
    artifact: &PlannedArtifact,
    work: &PendingWork,
    input: PreparedRepresentation,
    layout: &OutputLayout,
    output_limit: u64,
    budget: Option<&mut AssetLoadBudget>,
    observer: &ExecutionObserver,
) -> WorkOutcome {
    let planned_diagnostics = artifact.diagnostics().to_vec();
    let preferred_replace = match work.preferred_target {
        PreparedTarget::Encode { replace } => replace,
        target => {
            return prepared_target_outcome(
                artifact,
                artifact.preferred_kind(),
                artifact.preferred_path(),
                planned_diagnostics,
                target,
            )
            .expect("only an encoding target reaches an extraction worker");
        }
    };
    let preferred = stage_content(
        layout,
        artifact.preferred_path(),
        false,
        &input,
        artifact.ordinal(),
        output_limit,
        budget,
        observer,
    );
    match preferred {
        Ok(staged) => WorkOutcome::Staged {
            kind: artifact.preferred_kind(),
            path: artifact.preferred_path().clone(),
            staged,
            replace: preferred_replace,
            diagnostics: planned_diagnostics,
        },
        Err(AttemptError::PreferredUnavailable { terminal, fallback }) => {
            if artifact.fallback_kind().is_none() || artifact.fallback_path().is_none() {
                return failed_outcome(artifact, terminal);
            }
            WorkOutcome::FallbackRequired {
                input,
                diagnostic: fallback,
            }
        }
        Err(AttemptError::Output) => {
            failed_outcome(artifact, ExtractionDiagnosticCode::OutputFailed)
        }
        Err(AttemptError::OutputLimit) => {
            failed_outcome(artifact, ExtractionDiagnosticCode::OutputLimitExceeded)
        }
        Err(AttemptError::Fatal(error)) => WorkOutcome::Fatal(*error),
    }
}

struct FallbackResolution<'value> {
    layout: &'value OutputLayout,
    existing: ExistingOutputPolicy,
    output_limit: u64,
    evidence_read_budget: &'value mut EvidenceReadBudget,
    observer: &'value ExecutionObserver,
}

fn resolve_fallback_outcome(
    artifact: &PlannedArtifact,
    work: &PendingWork,
    outcome: WorkOutcome,
    resolution: FallbackResolution<'_>,
) -> WorkOutcome {
    let WorkOutcome::FallbackRequired { input, diagnostic } = outcome else {
        return outcome;
    };
    let (Some(kind), Some(path)) = (artifact.fallback_kind(), artifact.fallback_path()) else {
        return WorkOutcome::Fatal(ExtractionExecutionError::PublicationJournalConflict {
            reason: "worker requested an unplanned extraction fallback",
        });
    };
    let diagnostics = artifact_diagnostics_with(artifact, Some(diagnostic));
    let target = match prepare_target(
        resolution.layout,
        path,
        resolution.existing,
        work.fallback_evidence,
        resolution.evidence_read_budget,
        resolution.observer,
    ) {
        Ok(target) => target,
        Err(error) => return WorkOutcome::Fatal(error),
    };
    let replace = match target {
        PreparedTarget::Encode { replace } => replace,
        target => {
            return prepared_target_outcome(artifact, kind, path, diagnostics, target)
                .expect("only an encoding target continues fallback extraction");
        }
    };
    match stage_content(
        resolution.layout,
        path,
        true,
        &input,
        artifact.ordinal(),
        resolution.output_limit,
        None,
        resolution.observer,
    ) {
        Ok(staged) => WorkOutcome::Staged {
            kind,
            path: path.clone(),
            staged,
            replace,
            diagnostics,
        },
        Err(AttemptError::Fatal(error)) => WorkOutcome::Fatal(*error),
        Err(AttemptError::PreferredUnavailable { .. } | AttemptError::Output) => {
            failed_outcome(artifact, ExtractionDiagnosticCode::OutputFailed)
        }
        Err(AttemptError::OutputLimit) => {
            failed_outcome(artifact, ExtractionDiagnosticCode::OutputLimitExceeded)
        }
    }
}

fn stage_content(
    layout: &OutputLayout,
    path: &ExtractionPath,
    fallback: bool,
    input: &PreparedRepresentation,
    _ordinal: u32,
    output_limit: u64,
    budget: Option<&mut AssetLoadBudget>,
    _observer: &ExecutionObserver,
) -> Result<StagedOutput, AttemptError> {
    #[cfg(all(test, feature = "decode"))]
    let _open_files = _observer.reserve_open_files(OPEN_FILES_PER_WORKER);
    let output = layout
        .path(path.as_str())
        .map_err(|_| AttemptError::Output)?;
    let mut staging = output.create_staging().map_err(|_| AttemptError::Output)?;
    let (result, exceeded) = {
        let mut writer = OutputLimitWriter::new(staging.writer(), output_limit);
        #[cfg(all(test, feature = "decode"))]
        let result = {
            let mut writer = _observer.observe_writer(_ordinal, &mut writer);
            if fallback {
                input.write_fallback(&mut writer)
            } else {
                input.write_preferred(&mut writer, budget)
            }
            .map_err(map_representation_write_error)
        };
        #[cfg(not(all(test, feature = "decode")))]
        let result = if fallback {
            input.write_fallback(&mut writer)
        } else {
            input.write_preferred(&mut writer, budget)
        }
        .map_err(map_representation_write_error);
        (result, writer.exceeded())
    };
    if exceeded {
        return Err(AttemptError::OutputLimit);
    }
    result?;
    staging.finish().map_err(|_| AttemptError::Output)
}

fn map_representation_write_error(error: RepresentationWriteError) -> AttemptError {
    match error {
        RepresentationWriteError::InvalidContent => AttemptError::PreferredUnavailable {
            terminal: ExtractionDiagnosticCode::DecodedUnavailable,
            fallback: ExtractionDiagnosticCode::DecodeFailedRawFallback,
        },
        #[cfg(not(feature = "decode"))]
        RepresentationWriteError::CapabilityUnavailable { .. } => {
            AttemptError::PreferredUnavailable {
                terminal: ExtractionDiagnosticCode::FeatureUnavailable,
                fallback: ExtractionDiagnosticCode::FeatureUnavailable,
            }
        }
        RepresentationWriteError::Output => AttemptError::Output,
        RepresentationWriteError::Budget(error) => AttemptError::Fatal(Box::new(error.into())),
        #[cfg(feature = "decode")]
        RepresentationWriteError::Allocation {
            resource,
            requested,
        } => AttemptError::Fatal(Box::new(ExtractionExecutionError::Allocation {
            resource,
            requested,
            unit: ExtractionAllocationUnit::Bytes,
        })),
    }
}

enum AttemptError {
    PreferredUnavailable {
        terminal: ExtractionDiagnosticCode,
        fallback: ExtractionDiagnosticCode,
    },
    Output,
    OutputLimit,
    Fatal(Box<ExtractionExecutionError>),
}

enum WorkOutcome {
    Receipt(ExtractionManifestArtifact),
    Staged {
        kind: ExtractionArtifactKind,
        path: ExtractionPath,
        staged: StagedOutput,
        replace: bool,
        diagnostics: Vec<ExtractionDiagnostic>,
    },
    FallbackRequired {
        input: PreparedRepresentation,
        diagnostic: ExtractionDiagnosticCode,
    },
    Fatal(ExtractionExecutionError),
}

fn failed_outcome(artifact: &PlannedArtifact, code: ExtractionDiagnosticCode) -> WorkOutcome {
    receipt_outcome(
        artifact,
        artifact.preferred_kind(),
        artifact.preferred_path(),
        ExtractionArtifactStatus::Failed,
        None,
        None,
        artifact_diagnostics_with(artifact, Some(code)),
    )
}

#[allow(clippy::too_many_arguments)]
fn receipt_outcome(
    artifact: &PlannedArtifact,
    kind: ExtractionArtifactKind,
    path: &ExtractionPath,
    status: ExtractionArtifactStatus,
    length: Option<u64>,
    digest: Option<DigestV1>,
    diagnostics: Vec<ExtractionDiagnostic>,
) -> WorkOutcome {
    match ExtractionManifestArtifact::new(
        artifact.ordinal(),
        artifact.address().clone(),
        kind,
        path.clone(),
        status,
        length,
        digest,
        diagnostics,
    ) {
        Ok(receipt) => WorkOutcome::Receipt(receipt),
        Err(error) => WorkOutcome::Fatal(error.into()),
    }
}

fn publish_ready(
    plan: &ExtractionPlan,
    _options: &ExtractionExecutionOptions,
    outcomes: &mut [Option<WorkOutcome>],
    artifact_offset: usize,
    state: &mut ExtractionPublication<'_, '_>,
    _observer: &ExecutionObserver,
) -> Result<(), ExtractionExecutionError> {
    for (local_index, outcome) in outcomes.iter_mut().enumerate() {
        let artifact_index = artifact_offset + local_index;
        let artifact = &plan.artifacts()[artifact_index];
        if state.stopped() {
            if let Some(WorkOutcome::Staged { staged, .. }) = outcome.take() {
                let _ = staged.discard();
            }
            state.record(stopped_receipt(artifact)?)?;
            continue;
        }

        let outcome = outcome
            .take()
            .ok_or(ExtractionExecutionError::MissingWorkerOutcome {
                ordinal: artifact.ordinal(),
            })?;
        match outcome {
            WorkOutcome::Receipt(receipt) => {
                state.record(receipt)?;
            }
            WorkOutcome::Staged {
                kind,
                path,
                staged,
                replace,
                diagnostics,
            } => {
                let next = state.remaining_output().checked_sub(staged.length());
                if next.is_none() {
                    let _ = staged.discard();
                    state.record(failed_receipt(
                        artifact,
                        ExtractionDiagnosticCode::OutputLimitExceeded,
                    )?)?;
                    continue;
                }
                let length = staged.length();
                let digest = staged.digest();
                let receipt = ExtractionManifestArtifact::new(
                    artifact.ordinal(),
                    artifact.address().clone(),
                    kind,
                    path,
                    ExtractionArtifactStatus::Written,
                    Some(length),
                    Some(digest),
                    diagnostics,
                )?;
                let publication = {
                    #[cfg(all(test, feature = "decode"))]
                    let _open_files = _observer
                        .reserve_open_files(SERIAL_OPEN_FILE_PEAK - EXECUTION_LOCK_OPEN_FILES);
                    state.publish(receipt, staged, replace)
                };
                match publication? {
                    ArtifactPublication::Published => {}
                    ArtifactPublication::NotPublished => {
                        state.record(failed_receipt(
                            artifact,
                            ExtractionDiagnosticCode::OutputFailed,
                        )?)?;
                    }
                }
            }
            WorkOutcome::FallbackRequired { .. } => {
                return Err(ExtractionExecutionError::PublicationJournalConflict {
                    reason: "unresolved extraction fallback reached publication",
                });
            }
            WorkOutcome::Fatal(error) => return Err(error),
        }
    }
    Ok(())
}

fn failed_receipt(
    artifact: &PlannedArtifact,
    code: ExtractionDiagnosticCode,
) -> Result<ExtractionManifestArtifact, ExtractionExecutionError> {
    ExtractionManifestArtifact::new(
        artifact.ordinal(),
        artifact.address().clone(),
        artifact.preferred_kind(),
        artifact.preferred_path().clone(),
        ExtractionArtifactStatus::Failed,
        None,
        None,
        artifact_diagnostics_with(artifact, Some(code)),
    )
    .map_err(Into::into)
}

fn stopped_receipt(
    artifact: &PlannedArtifact,
) -> Result<ExtractionManifestArtifact, ExtractionExecutionError> {
    failed_receipt(artifact, ExtractionDiagnosticCode::StoppedAfterFailure)
}

fn artifact_diagnostics_with(
    artifact: &PlannedArtifact,
    additional: Option<ExtractionDiagnosticCode>,
) -> Vec<ExtractionDiagnostic> {
    let mut diagnostics = artifact.diagnostics().to_vec();
    if let Some(code) = additional {
        diagnostics.push(ExtractionDiagnostic::new(
            code,
            Some(artifact.address().clone()),
        ));
    }
    diagnostics
}

fn validate_actual_report(
    report: &ExtractionReport,
    limit: u64,
) -> Result<(), ExtractionExecutionError> {
    let required = canonical_length(report)?;
    if required > limit {
        return Err(ExtractionExecutionError::ReportLimitExceeded { required, limit });
    }
    Ok(())
}

fn canonical_length(value: &impl Serialize) -> Result<u64, ExtractionExecutionError> {
    let mut counter = CheckedByteCounter::new("report length overflow");
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| ExtractionExecutionError::ReportSerialization(error.to_string()))?;
    Ok(counter.bytes())
}

struct OutputLimitWriter<'writer> {
    inner: &'writer mut dyn Write,
    remaining: u64,
    exceeded: bool,
}

impl<'writer> OutputLimitWriter<'writer> {
    fn new(inner: &'writer mut dyn Write, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            exceeded: false,
        }
    }

    const fn exceeded(&self) -> bool {
        self.exceeded
    }
}

impl Write for OutputLimitWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        if length > self.remaining {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "staged output exceeds its deterministic limit",
            ));
        }
        let written = self.inner.write(buffer)?;
        let written_u64 = u64::try_from(written).unwrap_or(u64::MAX);
        self.remaining = self
            .remaining
            .checked_sub(written_u64)
            .ok_or_else(|| io::Error::other("staged output length overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Default)]
struct PreparationOrder {
    next: Mutex<usize>,
    changed: Condvar,
}

impl PreparationOrder {
    fn run<T>(
        &self,
        position: usize,
        prepare: impl FnOnce() -> Result<T, ExtractionExecutionError>,
    ) -> Result<T, ExtractionExecutionError> {
        let mut next = lock_recover(&self.next);
        while *next != position {
            next = wait_recover(&self.changed, next);
        }
        let result = prepare();
        *next += 1;
        self.changed.notify_all();
        result
    }

    fn skip(&self, position: usize) {
        let mut next = lock_recover(&self.next);
        while *next < position {
            next = wait_recover(&self.changed, next);
        }
        if *next == position {
            *next += 1;
            self.changed.notify_all();
        }
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_recover<'value, T>(
    condvar: &Condvar,
    guard: std::sync::MutexGuard<'value, T>,
) -> std::sync::MutexGuard<'value, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    fn pending(artifact_index: usize, working_set_bytes: u64) -> PendingWork {
        PendingWork {
            artifact_index,
            working_set_bytes,
            preferred_target: PreparedTarget::Encode { replace: false },
            fallback_evidence: None,
        }
    }

    #[test]
    fn pending_batches_prove_byte_and_open_file_peaks() {
        let pending = [
            pending(0, 6),
            pending(1, 4),
            pending(2, 7),
            pending(3, 3),
            pending(4, 10),
        ];
        let limits = ExtractionExecutionLimits::new(8, 10, 5, 1024, 1024, 1024).unwrap();

        let first = PendingBatch::select(limits, &pending, 0).unwrap();
        let second = PendingBatch::select(limits, &pending, first.end).unwrap();
        let third = PendingBatch::select(limits, &pending, second.end).unwrap();

        assert_eq!(
            [first, second, third],
            [
                PendingBatch {
                    end: 2,
                    working_set_bytes: 10,
                    open_files: 5,
                },
                PendingBatch {
                    end: 4,
                    working_set_bytes: 10,
                    open_files: 5,
                },
                PendingBatch {
                    end: 5,
                    working_set_bytes: 10,
                    open_files: 5,
                },
            ]
        );
        assert!(
            [first, second, third]
                .iter()
                .all(|batch| batch.working_set_bytes <= limits.max_in_flight_bytes())
        );
        assert!(
            [first, second, third]
                .iter()
                .all(|batch| batch.open_files <= limits.max_open_files())
        );
    }

    #[test]
    fn open_file_budget_limits_a_batch_even_when_bytes_do_not() {
        let pending = [pending(0, 1), pending(1, 1), pending(2, 1), pending(3, 1)];
        let limits = ExtractionExecutionLimits::new(8, 1024, 7, 1024, 1024, 1024).unwrap();

        let batch = PendingBatch::select(limits, &pending, 0).unwrap();

        assert_eq!(batch.end, 3);
        assert_eq!(batch.working_set_bytes, 3);
        assert_eq!(batch.open_files, 7);
    }
}

#[cfg(all(test, feature = "decode"))]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::publication::{PublicationCrashPoint, RECEIPT_SEGMENT_DIRECTORY, crash_once};
    use super::super::test_probe::ExecutionProbe;
    use super::*;
    use crate::extraction::{
        ExtractionFilter, ExtractionPlanner, ExtractionRepresentationPolicy, ExtractionRequest,
    };
    use crate::schema::SchemaRecipePlanner;
    use crate::workspace::{
        AssetWorkspace, MutationPlanBuilder, MutationValue, PrepareOptions, PreparedView,
        WorkspaceSnapshot, WorkspaceView,
    };
    use unity_asset_binary::asset::class_ids;
    use unity_asset_core::FieldPath;

    #[test]
    fn executor_rejects_tampered_stream_ranges_before_creating_output() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = media_plan(&directory);
        let mut wire = serde_json::to_value(&plan).unwrap();
        let stream = wire["artifacts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find_map(|artifact| {
                artifact["preferred_content"]["stream"]
                    .is_object()
                    .then(|| {
                        artifact["preferred_content"]["stream"]
                            .as_object_mut()
                            .unwrap()
                    })
            })
            .expect("media fixture must include a streamed artifact");
        let request = stream["request"].as_object_mut().unwrap();
        let offset = request["offset"].as_u64().unwrap();
        request.insert("offset".to_owned(), serde_json::Value::from(offset + 1));
        let encoded = serde_json::to_vec(&wire).unwrap();
        let tampered =
            ExtractionPlan::read_json(encoded.as_slice(), &mut AssetLoadBudget::default()).unwrap();
        let output = directory.path().join("tampered-output");

        let error = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &tampered,
                &output,
                ExtractionRunOptions::new(ExtractionExecutionOptions::default()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ExtractionExecutionError::PlanVerification(source)
                if matches!(
                    source.as_ref(),
                    ExtractionPlanError::PlanDerivationMismatch {
                        kind: super::super::planning_contract::ExtractionPlanMismatchKind::Representations,
                    }
                )
        ));
        assert!(!output.exists());
    }

    #[test]
    fn executor_rejects_tampered_sprite_texture_proofs_before_creating_output() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan, sprite_index) = sprite_plan();
        let mut texture_wire = serde_json::to_value(&plan).unwrap();
        let texture = &mut texture_wire["artifacts"][sprite_index]["preferred_content"]["texture"];
        let path_id = texture["path_id"].as_i64().unwrap();
        texture["path_id"] = serde_json::Value::from(path_id.saturating_add(1));
        let texture_plan = ExtractionPlan::read_json(
            serde_json::to_vec(&texture_wire).unwrap().as_slice(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_plan_representation_rejected(
            &snapshot,
            &texture_plan,
            &directory.path().join("tampered-texture"),
        );
    }

    #[test]
    fn sprite_working_set_has_an_exact_execution_boundary() {
        let (view, plan, sprite_index) = sprite_plan();
        assert_eq!(sprite_index, 0);
        let artifact = &plan.artifacts()[sprite_index];
        let declared = artifact.working_set_bytes();
        assert!(declared > 1);

        let mut budget = AssetLoadBudget::default();
        let context = RepresentationRuntimeContext::load(
            &view,
            plan.artifacts()
                .iter()
                .map(|artifact| artifact.representation()),
            &mut budget,
        )
        .unwrap();
        let runtime = context.bind(&view, &mut budget).unwrap();
        let exact_limits =
            ExtractionExecutionLimits::new(1, declared, 5, u64::MAX, u64::MAX, u64::MAX).unwrap();
        let proven = prove_working_sets(&runtime, &plan, exact_limits, &mut budget).unwrap();
        assert_eq!(proven, [declared]);

        let one_short =
            ExtractionExecutionLimits::new(1, declared - 1, 5, u64::MAX, u64::MAX, u64::MAX)
                .unwrap();
        let error = prove_working_sets(&runtime, &plan, one_short, &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::WorkingSetExceedsLimit {
                ordinal: 0,
                required,
                limit,
            } if required == declared && limit == declared - 1
        ));
    }

    #[test]
    fn stop_in_plan_order_does_not_start_workers_after_a_preflight_failure() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = media_plan(&directory);
        let output = directory.path().join("preflight-stop");
        write_existing(&output, plan.artifacts()[0].preferred_path(), b"occupied");
        let in_flight_limit = plan
            .artifacts()
            .iter()
            .map(PlannedArtifact::working_set_bytes)
            .max()
            .unwrap();
        let probe = ExecutionProbe::new([], []);

        let report = ExtractionExecutor::observing(Arc::clone(&probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(media_options(
                    8,
                    in_flight_limit,
                    ExtractionFailurePolicy::StopInPlanOrder,
                )),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert!(probe.snapshot().started_ordinals.is_empty());
        assert_eq!(
            report.manifest().artifacts()[0].diagnostics()[0].code(),
            ExtractionDiagnosticCode::OutputExists
        );
        assert!(report.manifest().artifacts()[1..].iter().all(|artifact| {
            artifact.diagnostics()[0].code() == ExtractionDiagnosticCode::StoppedAfterFailure
        }));
    }

    #[test]
    fn existing_outputs_are_hashed_only_for_resume_and_skip_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let artifact = &plan.artifacts()[0];
        let output = directory.path().join("existing-output-policy");
        write_existing(&output, artifact.preferred_path(), b"stale output");

        let error_probe = ExecutionProbe::new([], []);
        let rejected = ExtractionExecutor::observing(Arc::clone(&error_probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options(
                    &plan,
                    u64::MAX,
                    ExistingOutputPolicy::Error,
                )),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(rejected.counts().failed(), 1);
        assert_eq!(error_probe.snapshot().preflight_hash_bytes, 0);

        let replace_probe = ExecutionProbe::new([], []);
        let written = ExtractionExecutor::observing(Arc::clone(&replace_probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options(
                    &plan,
                    u64::MAX,
                    ExistingOutputPolicy::Replace,
                )),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(written.counts().written(), 1);
        assert_eq!(replace_probe.snapshot().preflight_hash_bytes, 0);
        let encoded_length = written.manifest().artifacts()[0].length().unwrap();

        let resume_probe = ExecutionProbe::new([], []);
        let resumed = ExtractionExecutor::observing(Arc::clone(&resume_probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options(
                    &plan,
                    u64::MAX,
                    ExistingOutputPolicy::Error,
                ))
                .with_resume(written.manifest()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(resumed.counts().resumed(), 1);
        assert_eq!(resume_probe.snapshot().preflight_hash_bytes, encoded_length);

        let skip_probe = ExecutionProbe::new([], []);
        let skipped = ExtractionExecutor::observing(Arc::clone(&skip_probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options(
                    &plan,
                    u64::MAX,
                    ExistingOutputPolicy::Skip,
                )),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(skipped.counts().skipped_existing(), 1);
        assert_eq!(skip_probe.snapshot().preflight_hash_bytes, encoded_length);

        let output_path = output.join(artifact.preferred_path().as_str());
        let mut corrupt = fs::read(&output_path).unwrap();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        fs::write(&output_path, &corrupt).unwrap();
        let mismatch_probe = ExecutionProbe::new([], []);
        let mismatched = ExtractionExecutor::observing(Arc::clone(&mismatch_probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options(
                    &plan,
                    encoded_length,
                    ExistingOutputPolicy::Skip,
                ))
                .with_resume(written.manifest()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(mismatched.counts().skipped_existing(), 1);
        assert_eq!(
            mismatch_probe.snapshot().preflight_hash_bytes,
            encoded_length
        );

        fs::write(&output_path, b"too large").unwrap();
        let bounded_probe = ExecutionProbe::new([], []);
        let error = ExtractionExecutor::observing(Arc::clone(&bounded_probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options_with_verification(
                    &plan,
                    u64::MAX,
                    1,
                    ExistingOutputPolicy::Skip,
                )),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::EvidenceVerificationLimitExceeded {
                required: 9,
                remaining: 1,
            }
        ));
        assert_eq!(bounded_probe.snapshot().preflight_hash_bytes, 0);
    }

    #[test]
    fn replace_policy_overwrites_a_target_created_after_preflight() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = media_plan(&directory);
        let artifact = &plan.artifacts()[0];
        let artifact_path = artifact.preferred_path().clone();
        let artifact_ordinal = artifact.ordinal();
        let output = directory.path().join("replace-publication-race");
        let probe = ExecutionProbe::new([artifact_ordinal], []);

        let report = thread::scope(|scope| {
            let _release_on_unwind = probe.release_on_drop();
            let executor = ExtractionExecutor::observing(Arc::clone(&probe));
            let worker_snapshot = &snapshot;
            let worker_plan = &plan;
            let worker_output = &output;
            let worker_options = output_options(&plan, u64::MAX, ExistingOutputPolicy::Replace);
            let worker = scope.spawn(move || {
                executor.execute(
                    worker_snapshot,
                    worker_plan,
                    worker_output,
                    ExtractionRunOptions::new(worker_options),
                    &mut AssetLoadBudget::default(),
                )
            });
            probe.wait_for_waiters(1);
            write_existing(&output, &artifact_path, b"racing output");
            probe.release_writes();
            worker.join().unwrap().unwrap()
        });

        let receipt = &report.manifest().artifacts()[usize::try_from(artifact_ordinal).unwrap()];
        assert_eq!(receipt.status(), ExtractionArtifactStatus::Written);
        let published = fs::read(output.join(artifact_path.as_str())).unwrap();
        assert_ne!(published, b"racing output");
        assert_eq!(
            u64::try_from(published.len()).unwrap(),
            receipt.length().unwrap()
        );
    }

    #[test]
    fn preferred_success_does_not_inspect_an_unused_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let (view, plan, artifact_index) = sprite_plan();
        let artifact = &plan.artifacts()[artifact_index];
        let fallback_path = artifact
            .fallback_path()
            .expect("prefer-decoded texture must retain a raw fallback");

        let measured_output = directory.path().join("fallback-measurement");
        let measured = ExtractionExecutor::new()
            .execute(
                &view,
                &plan,
                &measured_output,
                ExtractionRunOptions::new(ExtractionExecutionOptions::default()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let preferred_length = measured.manifest().artifacts()[0].length().unwrap();

        let output = directory.path().join("unused-fallback");
        let fallback_output = output.join(fallback_path.as_str());
        fs::create_dir_all(fallback_output.parent().unwrap()).unwrap();
        fs::File::create(&fallback_output)
            .unwrap()
            .set_len(preferred_length.checked_add(1).unwrap())
            .unwrap();
        let probe = ExecutionProbe::new([], []);
        let options = output_options_with_verification(
            &plan,
            u64::MAX,
            preferred_length,
            ExistingOutputPolicy::Skip,
        );

        let report = ExtractionExecutor::observing(Arc::clone(&probe))
            .execute(
                &view,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(report.counts().written(), 1);
        assert_eq!(
            report.manifest().artifacts()[0].path(),
            artifact.preferred_path()
        );
        assert_eq!(probe.snapshot().preflight_hash_bytes, 0);
    }

    #[test]
    fn artifact_move_without_ack_is_reconciled_without_republishing() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let output = directory.path().join("artifact-move-recovery");
        let options = ExtractionExecutionOptions::default();

        let error = {
            let _crash = crash_once(PublicationCrashPoint::ArtifactMoved(0));
            ExtractionExecutor::new().execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
        }
        .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationRecoveryRequired {
                stage: "artifact_publication"
            }
        ));
        let artifact_path = output.join(plan.artifacts()[0].preferred_path().as_str());
        let published = fs::read(&artifact_path).unwrap();

        let report = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(report.counts().written(), 1);
        assert_eq!(fs::read(artifact_path).unwrap(), published);
        assert!(output.join(PUBLICATION_JOURNAL_PATH).is_file());
    }

    #[test]
    fn pending_existing_receipt_is_revalidated_before_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let output = directory.path().join("pending-existing-revalidation");
        let artifact_path = plan.artifacts()[0].preferred_path();
        write_existing(&output, artifact_path, b"four");
        let options = output_options(&plan, u64::MAX, ExistingOutputPolicy::Skip);

        let error = {
            let _crash = crash_once(PublicationCrashPoint::ReceiptPersisted(0));
            ExtractionExecutor::new().execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
        }
        .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationRecoveryRequired {
                stage: "receipt_commit"
            }
        ));

        fs::write(output.join(artifact_path.as_str()), b"five").unwrap();
        let error = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationJournalConflict { .. }
        ));
    }

    #[test]
    fn recovered_failure_receipt_preserves_stop_in_plan_order() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = media_plan(&directory);
        assert!(plan.artifacts().len() >= 2);
        let output = directory.path().join("pending-failure-stop");
        write_existing(&output, plan.artifacts()[0].preferred_path(), b"occupied");
        let options = output_options(&plan, u64::MAX, ExistingOutputPolicy::Error);

        {
            let _crash = crash_once(PublicationCrashPoint::ReceiptPersisted(0));
            let error = ExtractionExecutor::new()
                .execute(
                    &snapshot,
                    &plan,
                    &output,
                    ExtractionRunOptions::new(options),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap_err();
            assert!(matches!(
                error,
                ExtractionExecutionError::PublicationRecoveryRequired {
                    stage: "receipt_commit"
                }
            ));
        }

        let report = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(
            report.manifest().artifacts()[0].status(),
            ExtractionArtifactStatus::Failed
        );
        assert_eq!(
            report.manifest().artifacts()[1].diagnostics()[0].code(),
            ExtractionDiagnosticCode::StoppedAfterFailure
        );
    }

    #[test]
    fn recovered_receipts_and_preflight_share_the_evidence_verification_limit() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = yaml_plan(&directory, 2);
        let output = directory.path().join("recovered-existing-hash-limit");
        for artifact in plan.artifacts().iter().take(2) {
            write_existing(&output, artifact.preferred_path(), b"four");
        }
        let options =
            output_options_with_verification(&plan, u64::MAX, 16, ExistingOutputPolicy::Skip);

        {
            let _crash = crash_once(PublicationCrashPoint::ReceiptPersisted(0));
            ExtractionExecutor::new()
                .execute(
                    &snapshot,
                    &plan,
                    &output,
                    ExtractionRunOptions::new(options),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap_err();
        }

        let report = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(
            report.manifest().artifacts()[0].status(),
            ExtractionArtifactStatus::SkippedExisting
        );
        assert_eq!(
            report.manifest().artifacts()[1].status(),
            ExtractionArtifactStatus::SkippedExisting
        );
    }

    #[test]
    fn evidence_verification_limit_is_cumulative_and_can_be_raised_for_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let exact_output = directory.path().join("verification-exact");
        write_existing(&exact_output, plan.artifacts()[0].preferred_path(), b"four");
        let exact =
            output_options_with_verification(&plan, u64::MAX, 8, ExistingOutputPolicy::Skip);
        let report = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &exact_output,
                ExtractionRunOptions::new(exact),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(
            report.manifest().artifacts()[0].status(),
            ExtractionArtifactStatus::SkippedExisting
        );

        let one_short_output = directory.path().join("verification-one-short");
        write_existing(
            &one_short_output,
            plan.artifacts()[0].preferred_path(),
            b"four",
        );
        let one_short =
            output_options_with_verification(&plan, u64::MAX, 7, ExistingOutputPolicy::Skip);
        let error = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &one_short_output,
                ExtractionRunOptions::new(one_short),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::EvidenceVerificationLimitExceeded {
                required: 4,
                remaining: 3,
            }
        ));

        let recovered = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &one_short_output,
                ExtractionRunOptions::new(exact),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(
            recovered.manifest().artifacts()[0].status(),
            ExtractionArtifactStatus::SkippedExisting
        );
    }

    #[test]
    fn skip_preflight_reports_evidence_verification_exhaustion_as_an_execution_error() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let output = directory.path().join("preflight-verification-limit");
        write_existing(&output, plan.artifacts()[0].preferred_path(), b"four");
        let options =
            output_options_with_verification(&plan, u64::MAX, 3, ExistingOutputPolicy::Skip);

        let error = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ExtractionExecutionError::EvidenceVerificationLimitExceeded {
                required: 4,
                remaining: 3,
            }
        ));
    }

    #[test]
    fn manifest_move_without_ack_is_reconciled_as_committed() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let output = directory.path().join("manifest-move-recovery");
        let manifest_path = ExtractionPath::new("manifest.json").unwrap();
        let options = ExtractionExecutionOptions::default();

        let error = {
            let _crash = crash_once(PublicationCrashPoint::ManifestMoved);
            ExtractionExecutor::new().execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options).with_manifest_path(&manifest_path),
                &mut AssetLoadBudget::default(),
            )
        }
        .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationRecoveryRequired {
                stage: "manifest_publication"
            }
        ));
        let manifest_before = fs::read(output.join(manifest_path.as_str())).unwrap();

        let report = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options).with_manifest_path(&manifest_path),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(report.counts().written(), 1);
        assert_eq!(
            fs::read(output.join(manifest_path.as_str())).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn publication_commit_rejects_artifact_changed_after_manifest_publication() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let output = directory.path().join("artifact-changed-before-commit");
        let manifest_path = ExtractionPath::new("manifest.json").unwrap();
        let probe = ExecutionProbe::new([], []);
        probe.block_publication_commit();
        let executor = ExtractionExecutor::observing(Arc::clone(&probe));

        let error = std::thread::scope(|scope| {
            let run = scope.spawn(|| {
                executor.execute(
                    &snapshot,
                    &plan,
                    &output,
                    ExtractionRunOptions::new(ExtractionExecutionOptions::default())
                        .with_manifest_path(&manifest_path),
                    &mut AssetLoadBudget::default(),
                )
            });
            probe.wait_for_publication_commit();
            assert!(output.join(manifest_path.as_str()).is_file());
            fs::write(
                output.join(plan.artifacts()[0].preferred_path().as_str()),
                b"changed after manifest publication",
            )
            .unwrap();
            probe.release_publication_commit();
            run.join().unwrap().unwrap_err()
        });

        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationJournalConflict { .. }
        ));
    }

    #[test]
    fn publication_commit_rejects_manifest_changed_after_publication() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let output = directory.path().join("manifest-changed-before-commit");
        let manifest_path = ExtractionPath::new("manifest.json").unwrap();
        let probe = ExecutionProbe::new([], []);
        probe.block_publication_commit();
        let executor = ExtractionExecutor::observing(Arc::clone(&probe));

        let error = std::thread::scope(|scope| {
            let run = scope.spawn(|| {
                executor.execute(
                    &snapshot,
                    &plan,
                    &output,
                    ExtractionRunOptions::new(ExtractionExecutionOptions::default())
                        .with_manifest_path(&manifest_path),
                    &mut AssetLoadBudget::default(),
                )
            });
            probe.wait_for_publication_commit();
            fs::write(
                output.join(manifest_path.as_str()),
                b"changed after manifest publication",
            )
            .unwrap();
            probe.release_publication_commit();
            run.join().unwrap().unwrap_err()
        });

        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationJournalConflict { .. }
        ));
    }

    #[test]
    fn committed_journal_replays_without_a_live_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let output = directory.path().join("committed-return-recovery");
        let options = ExtractionExecutionOptions::default();

        let error = {
            let _crash = crash_once(PublicationCrashPoint::CommittedPersisted);
            ExtractionExecutor::new().execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
        }
        .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationRecoveryRequired {
                stage: "committed_return"
            }
        ));
        let unrelated = AssetWorkspace::new().unwrap().snapshot();

        let report = ExtractionExecutor::new()
            .execute(
                &unrelated,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(report.counts().written(), 1);
    }

    #[test]
    fn pending_publication_rejects_changed_bytes_and_execution_intent() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = texture_plan();
        let output = directory.path().join("pending-conflict");
        let options = ExtractionExecutionOptions::default();

        {
            let _crash = crash_once(PublicationCrashPoint::ArtifactMoved(0));
            ExtractionExecutor::new()
                .execute(
                    &snapshot,
                    &plan,
                    &output,
                    ExtractionRunOptions::new(options),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap_err();
        }
        fs::write(
            output.join(plan.artifacts()[0].preferred_path().as_str()),
            b"changed after an uncertain move",
        )
        .unwrap();
        let changed = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            changed,
            ExtractionExecutionError::PublicationJournalConflict { .. }
        ));

        let changed_options = ExtractionExecutionOptions::new(
            options.limits(),
            options.existing_output(),
            ExtractionFailurePolicy::StopInPlanOrder,
        )
        .unwrap();
        let changed_intent = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(changed_options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            changed_intent,
            ExtractionExecutionError::PublicationJournalConflict { .. }
        ));
    }

    #[test]
    fn receipt_segments_preserve_a_committed_report_with_a_sealed_segment_and_tail() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = yaml_plan(&directory, 65);
        let output = directory.path().join("sealed-segment-and-tail");

        let report = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(ExtractionExecutionOptions::default()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(report.manifest().artifacts().len(), 65);
        assert_eq!(receipt_segment_count(&output), 1);

        let unrelated = AssetWorkspace::new().unwrap().snapshot();
        let replayed = ExtractionExecutor::new()
            .execute(
                &unrelated,
                &plan,
                &output,
                ExtractionRunOptions::new(ExtractionExecutionOptions::default()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(
            replayed.canonical_manifest_json().unwrap(),
            report.canonical_manifest_json().unwrap()
        );
    }

    #[test]
    fn segment_move_without_header_ack_is_rebuilt_from_the_authoritative_tail() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = yaml_plan(&directory, 64);
        let output = directory.path().join("segment-move-recovery");
        let options = ExtractionExecutionOptions::default();

        let error = {
            let _crash = crash_once(PublicationCrashPoint::SegmentMoved(0));
            ExtractionExecutor::new().execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
        }
        .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationRecoveryRequired {
                stage: "receipt_segment_seal"
            }
        ));
        assert_eq!(receipt_segment_count(&output), 1);

        let report = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(report.manifest().artifacts().len(), 64);
        assert_eq!(receipt_segment_count(&output), 1);
    }

    #[test]
    fn committed_publication_rejects_a_corrupt_receipt_segment() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = yaml_plan(&directory, 64);
        let output = directory.path().join("corrupt-segment");
        let options = ExtractionExecutionOptions::default();

        ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        fs::write(
            output.join(RECEIPT_SEGMENT_DIRECTORY).join("00000000.json"),
            b"{}",
        )
        .unwrap();

        let error = ExtractionExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(options),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationJournalInvalid { .. }
        ));
    }

    #[test]
    fn evidence_verification_limit_is_cumulative_across_preflight() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = media_plan(&directory);
        let output = directory.path().join("cumulative-existing-hash");
        for artifact in plan.artifacts() {
            write_existing(&output, artifact.preferred_path(), b"four");
        }
        let probe = ExecutionProbe::new([], []);

        let error = ExtractionExecutor::observing(Arc::clone(&probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options_with_verification(
                    &plan,
                    u64::MAX,
                    7,
                    ExistingOutputPolicy::Skip,
                )),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ExtractionExecutionError::EvidenceVerificationLimitExceeded {
                required: 4,
                remaining: 3,
            }
        ));
        assert_eq!(probe.snapshot().preflight_hash_bytes, 4);
    }

    #[test]
    fn slow_and_failing_media_sinks_preserve_resource_bounds_and_plan_order() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = media_plan(&directory);
        assert!(plan.artifacts().len() >= 3);
        assert_eq!(
            plan.artifacts()[0].preferred_kind(),
            ExtractionArtifactKind::TexturePng
        );
        assert_eq!(
            plan.artifacts()[1].preferred_kind(),
            ExtractionArtifactKind::Audio
        );

        let first_batch_bytes = plan.artifacts()[0]
            .working_set_bytes()
            .checked_add(plan.artifacts()[1].working_set_bytes())
            .unwrap();
        let max_artifact_bytes = plan
            .artifacts()
            .iter()
            .map(PlannedArtifact::working_set_bytes)
            .max()
            .unwrap();
        let in_flight_limit = first_batch_bytes.max(max_artifact_bytes);
        let many_options = media_options(8, in_flight_limit, ExtractionFailurePolicy::CollectAll);
        let manifest_path = ExtractionPath::new("manifest.json").unwrap();

        let slow_probe = ExecutionProbe::new([0, 1], []);
        let slow_output = directory.path().join("slow-success");
        let slow_report = thread::scope(|scope| {
            let _release_on_unwind = slow_probe.release_on_drop();
            let executor = ExtractionExecutor::observing(Arc::clone(&slow_probe));
            let snapshot = &snapshot;
            let plan = &plan;
            let slow_output = &slow_output;
            let many_options = &many_options;
            let manifest_path = &manifest_path;
            let worker = scope.spawn(move || {
                executor.execute(
                    snapshot,
                    plan,
                    slow_output,
                    ExtractionRunOptions::new(*many_options).with_manifest_path(manifest_path),
                    &mut AssetLoadBudget::default(),
                )
            });
            slow_probe.wait_for_waiters(2);
            let blocked = slow_probe.snapshot();
            let mut started = blocked.started_ordinals.clone();
            started.sort_unstable();
            assert_eq!(started, [0, 1]);
            assert_eq!(blocked.active_working_set_bytes, first_batch_bytes);
            assert_eq!(blocked.peak_working_set_bytes, first_batch_bytes);
            assert_eq!(blocked.active_open_files, 5);
            assert_eq!(blocked.peak_open_files, 5);
            slow_probe.release_writes();
            worker.join().unwrap().unwrap()
        });
        let completed = slow_probe.snapshot();
        assert_eq!(completed.active_working_set_bytes, 0);
        assert_eq!(completed.active_open_files, 0);
        assert!(completed.peak_working_set_bytes <= in_flight_limit);
        assert!(completed.peak_open_files <= many_options.limits().max_open_files());
        assert!(
            slow_report
                .manifest()
                .artifacts()
                .iter()
                .all(|artifact| artifact.status() == ExtractionArtifactStatus::Written)
        );
        assert_plan_order(&slow_report);
        assert!(slow_output.join(manifest_path.as_str()).is_file());

        let stop_options =
            media_options(8, in_flight_limit, ExtractionFailurePolicy::StopInPlanOrder);
        let failing_probe = ExecutionProbe::new([0], [1]);
        let many_failure_output = directory.path().join("many-failure");
        let many_failure = thread::scope(|scope| {
            let _release_on_unwind = failing_probe.release_on_drop();
            let executor = ExtractionExecutor::observing(Arc::clone(&failing_probe));
            let snapshot = &snapshot;
            let plan = &plan;
            let many_failure_output = &many_failure_output;
            let stop_options = &stop_options;
            let manifest_path = &manifest_path;
            let worker = scope.spawn(move || {
                executor.execute(
                    snapshot,
                    plan,
                    many_failure_output,
                    ExtractionRunOptions::new(*stop_options).with_manifest_path(manifest_path),
                    &mut AssetLoadBudget::default(),
                )
            });
            failing_probe.wait_for_waiters(1);
            failing_probe.wait_for_failures(1);
            let reversed = failing_probe.snapshot();
            let mut started = reversed.started_ordinals.clone();
            started.sort_unstable();
            assert_eq!(started, [0, 1]);
            assert_eq!(reversed.peak_working_set_bytes, first_batch_bytes);
            assert_eq!(reversed.peak_open_files, 5);
            failing_probe.release_writes();
            worker.join().unwrap().unwrap()
        });
        assert_plan_order(&many_failure);
        assert_eq!(
            many_failure.manifest().artifacts()[0].status(),
            ExtractionArtifactStatus::Written
        );
        assert_eq!(
            many_failure.manifest().artifacts()[1].status(),
            ExtractionArtifactStatus::Failed
        );
        assert_eq!(
            many_failure.manifest().artifacts()[1].diagnostics()[0].code(),
            ExtractionDiagnosticCode::OutputFailed
        );
        for artifact in &many_failure.manifest().artifacts()[2..] {
            assert_eq!(artifact.status(), ExtractionArtifactStatus::Failed);
            assert_eq!(
                artifact.diagnostics()[0].code(),
                ExtractionDiagnosticCode::StoppedAfterFailure
            );
        }

        let single_probe = ExecutionProbe::new([], [1]);
        let single_failure = ExtractionExecutor::observing(Arc::clone(&single_probe))
            .execute(
                &snapshot,
                &plan,
                &directory.path().join("single-failure"),
                ExtractionRunOptions::new(media_options(
                    1,
                    in_flight_limit,
                    ExtractionFailurePolicy::StopInPlanOrder,
                ))
                .with_manifest_path(&manifest_path),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(
            many_failure.canonical_manifest_json().unwrap(),
            single_failure.canonical_manifest_json().unwrap()
        );
    }

    fn media_plan(directory: &tempfile::TempDir) -> (WorkspaceSnapshot, ExtractionPlan) {
        for (target, source) in [
            ("a-banner.assets", "banner_1"),
            ("b-audio.ab", "char_118_yuki.ab"),
            ("c-banner.assets", "banner_1"),
        ] {
            fs::copy(sample(source), directory.path().join(target)).unwrap();
        }

        let mut workspace = AssetWorkspace::new().unwrap();
        for source in ["a-banner.assets", "b-audio.ab", "c-banner.assets"] {
            workspace
                .load_path(
                    directory.path().join(source),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap();
        }
        let snapshot = workspace.snapshot();
        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
            .with_filter(
                ExtractionFilter::new(
                    [class_ids::TEXTURE_2D, class_ids::AUDIO_CLIP],
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
        let plan = ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();
        (snapshot, plan)
    }

    fn yaml_plan(
        directory: &tempfile::TempDir,
        count: usize,
    ) -> (WorkspaceSnapshot, ExtractionPlan) {
        let mut source = String::from("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n");
        for index in 1..=count {
            source.push_str(&format!(
                "--- !u!1 &{index}\nGameObject:\n  m_Name: Object{index}\n"
            ));
        }
        let source_path = directory.path().join(format!("objects-{count}.prefab"));
        fs::write(&source_path, source).unwrap();

        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_path(&source_path, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let plan = ExtractionPlanner::new(&snapshot)
            .plan(
                ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(plan.artifacts().len(), count);
        (snapshot, plan)
    }

    fn receipt_segment_count(output: &Path) -> usize {
        let directory = output.join(RECEIPT_SEGMENT_DIRECTORY);
        if !directory.exists() {
            return 0;
        }
        fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .count()
    }

    fn sprite_plan() -> (PreparedView, ExtractionPlan, usize) {
        let mut workspace = AssetWorkspace::new().unwrap();
        for name in [
            "atlas_test",
            "banner_1",
            "char_118_yuki.ab",
            "xinzexi_2_n_tex",
        ] {
            workspace
                .load_path(sample(name), &mut AssetLoadBudget::default())
                .unwrap_or_else(|error| panic!("failed to load {name}: {error}"));
        }
        let snapshot = workspace.snapshot();
        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::PreferDecoded)
            .with_filter(
                ExtractionFilter::new([class_ids::SPRITE], None, Some("banner_1".to_owned()), None)
                    .unwrap(),
            );
        let raw_plan = ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();
        let address = raw_plan
            .artifacts()
            .iter()
            .find(|artifact| {
                artifact.diagnostics().iter().any(|diagnostic| {
                    diagnostic.code() == ExtractionDiagnosticCode::UnsupportedMediaLayout
                })
            })
            .expect("fixture must contain a packed Sprite")
            .address()
            .clone();

        let recipe_planner = SchemaRecipePlanner::new(&snapshot);
        let sprite = recipe_planner
            .inspect(&address, &mut AssetLoadBudget::default())
            .unwrap();
        let mut builder = MutationPlanBuilder::new(snapshot.workspace_id(), snapshot.revision());
        for (path, replacement) in [
            (
                field_path(&["m_RD", "settingsRaw"]),
                MutationValue::unsigned(0),
            ),
            (
                field_path(&["m_AtlasTags"]),
                MutationValue::array(Vec::new()).unwrap(),
            ),
            (
                field_path(&["m_RD", "downscaleMultiplier"]),
                MutationValue::float64(1.0),
            ),
            (
                field_path(&["m_RD", "textureRect", "x"]),
                MutationValue::float64(0.0),
            ),
            (
                field_path(&["m_RD", "textureRect", "y"]),
                MutationValue::float64(0.0),
            ),
            (
                field_path(&["m_RD", "textureRect", "width"]),
                MutationValue::float64(1.0),
            ),
            (
                field_path(&["m_RD", "textureRect", "height"]),
                MutationValue::float64(1.0),
            ),
        ] {
            if sprite.field(&path).is_none() {
                continue;
            }
            let fragment = recipe_planner
                .lower_field_replace(&sprite, path, replacement, &mut AssetLoadBudget::default())
                .unwrap();
            builder.append(fragment).unwrap();
        }
        let prepared = workspace
            .prepare(
                builder.build().unwrap(),
                PrepareOptions::default(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let view = prepared.view();
        let request =
            ExtractionRequest::addresses([address], ExtractionRepresentationPolicy::PreferDecoded)
                .unwrap();
        let plan = ExtractionPlanner::new(&view)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();
        assert_eq!(plan.artifacts().len(), 1);
        let sprite_index = 0;
        assert_eq!(
            plan.artifacts()[sprite_index].preferred_kind(),
            ExtractionArtifactKind::SpritePng
        );
        assert_eq!(
            plan.artifacts()[sprite_index].fallback_kind(),
            Some(ExtractionArtifactKind::BinaryRaw)
        );
        (view, plan, sprite_index)
    }

    fn field_path(fields: &[&str]) -> FieldPath {
        fields.iter().fold(FieldPath::root(), |path, field| {
            path.push_field(*field).unwrap()
        })
    }

    fn texture_plan() -> (WorkspaceSnapshot, ExtractionPlan) {
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_path(sample("banner_1"), &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
            .with_filter(ExtractionFilter::new([class_ids::TEXTURE_2D], None, None, None).unwrap());
        let plan = ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();
        assert_eq!(plan.artifacts().len(), 1);
        (snapshot, plan)
    }

    fn assert_plan_representation_rejected(
        snapshot: &impl WorkspaceView,
        plan: &ExtractionPlan,
        output: &Path,
    ) {
        let error = ExtractionExecutor::new()
            .execute(
                snapshot,
                plan,
                output,
                ExtractionRunOptions::new(ExtractionExecutionOptions::default()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::PlanVerification(source)
                if matches!(
                    source.as_ref(),
                    ExtractionPlanError::PlanDerivationMismatch {
                        kind: super::super::planning_contract::ExtractionPlanMismatchKind::Representations,
                    }
                )
        ));
        assert!(!output.exists());
    }

    fn write_existing(root: &Path, path: &ExtractionPath, bytes: &[u8]) {
        let output = root.join(path.as_str());
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(output, bytes).unwrap();
    }

    fn output_options(
        plan: &ExtractionPlan,
        output_limit: u64,
        existing: ExistingOutputPolicy,
    ) -> ExtractionExecutionOptions {
        output_options_with_verification(plan, output_limit, u64::MAX, existing)
    }

    fn output_options_with_verification(
        plan: &ExtractionPlan,
        output_limit: u64,
        evidence_verification_limit: u64,
        existing: ExistingOutputPolicy,
    ) -> ExtractionExecutionOptions {
        let in_flight = plan
            .artifacts()
            .iter()
            .map(PlannedArtifact::working_set_bytes)
            .max()
            .unwrap();
        ExtractionExecutionOptions::new(
            ExtractionExecutionLimits::new(
                1,
                in_flight,
                5,
                output_limit,
                evidence_verification_limit,
                16 * 1024 * 1024,
            )
            .unwrap(),
            existing,
            ExtractionFailurePolicy::StopInPlanOrder,
        )
        .unwrap()
    }

    fn media_options(
        workers: usize,
        in_flight_bytes: u64,
        failure: ExtractionFailurePolicy,
    ) -> ExtractionExecutionOptions {
        ExtractionExecutionOptions::new(
            ExtractionExecutionLimits::new(
                workers,
                in_flight_bytes,
                5,
                2 * 1024 * 1024 * 1024,
                u64::MAX,
                16 * 1024 * 1024,
            )
            .unwrap(),
            ExistingOutputPolicy::Error,
            failure,
        )
        .unwrap()
    }

    fn assert_plan_order(report: &ExtractionReport) {
        assert_eq!(
            report
                .manifest()
                .artifacts()
                .iter()
                .map(ExtractionManifestArtifact::ordinal)
                .collect::<Vec<_>>(),
            (0..u32::try_from(report.manifest().artifacts().len()).unwrap()).collect::<Vec<_>>()
        );
    }

    fn sample(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples")
            .join(name)
    }
}
