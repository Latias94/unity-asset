use std::io::{self, Write};
use std::path::Path;
#[cfg(feature = "decode")]
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

use serde::Serialize;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, ObjectAddress, SourceLocator, UnityValue,
    vec_allocation_bytes,
};
use unity_asset_yaml::UnityYamlSerializer;

#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipConverter, AudioExporter, AudioSourceError, PreparedAudioSource},
    sprite::{DecodedSpriteTexture, SpriteProcessor},
    texture::{TextureExporter, TextureProcessor},
};

use super::CheckedByteCounter;
use super::artifact::{ExtractionOutputErrorKind, OutputArtifactError, OutputLayout, StagedOutput};
use super::manifest::{
    ExtractionArtifactStatus, ExtractionCanonicalError, ExtractionDiagnostic,
    ExtractionDiagnosticCode, ExtractionManifest, ExtractionManifestArtifact,
    ExtractionManifestError, ExtractionReport, maximum_extraction_report,
};
#[cfg(feature = "decode")]
use super::model::ExtractionSourceRange;
use super::model::{
    ExtractionArtifactKind, ExtractionPath, ExtractionPlan, PlannedArtifact, PlannedContent,
};
#[cfg(feature = "decode")]
use super::reservation::requires_stream_resolution;
use super::reservation::{ExtractionReservationError, trusted_working_set};
use super::source_budget_error;
#[cfg(feature = "decode")]
use crate::reference::ReferenceGraphError;
#[cfg(feature = "decode")]
use crate::workspace::StreamedResourceResolver;
use crate::workspace::{
    WorkspaceError, WorkspaceLookup, WorkspaceObject, WorkspaceObjectValue, WorkspaceView,
};

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
        max_report_bytes: u64,
    ) -> Result<Self, ExtractionExecutionError> {
        let limits = Self {
            workers,
            max_in_flight_bytes,
            max_open_files,
            max_output_bytes,
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
    ///
    /// The same independent ceiling bounds the total existing-output bytes read
    /// for skip and resume evidence, so preflight cannot perform unbounded I/O.
    pub const fn max_output_bytes(self) -> u64 {
        self.max_output_bytes
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
            max_report_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Runtime-only execution choices. None of these fields enter canonical evidence.
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
    fn record_existing_hash(&self, bytes: u64) {
        super::test_probe::record_existing_hash(self.probe(), bytes);
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
        validate_context(view, plan)?;
        validate_sources(view, plan, budget)?;
        validate_resume(plan, resume)?;
        let working_sets = prove_working_sets(view, plan, options.limits, budget)?;
        let manifest_output_reservation =
            validate_contract_bounds(plan, options.limits, manifest_path.is_some())?;
        let artifact_output_limit = options
            .limits
            .max_output_bytes
            .checked_sub(manifest_output_reservation)
            .expect("validated manifest reservation exceeds the output limit");

        let relative_paths = plan
            .artifacts()
            .iter()
            .flat_map(|artifact| {
                std::iter::once(artifact.preferred_path().as_str())
                    .chain(artifact.fallback_path().map(ExtractionPath::as_str))
            })
            .chain(manifest_path.into_iter().map(ExtractionPath::as_str));
        let layout = OutputLayout::prepare(output_root, relative_paths)
            .map_err(ExtractionExecutionError::output_layout)?;
        #[cfg(all(test, feature = "decode"))]
        let _execution_open_file = self.observer.reserve_open_files(EXECUTION_LOCK_OPEN_FILES);

        let mut outcomes = (0..plan.artifacts().len())
            .map(|_| None)
            .collect::<Vec<Option<WorkOutcome>>>();
        let mut pending = Vec::new();
        let mut preflight_stopped = false;
        let mut remaining_existing_hash_bytes = options.limits.max_output_bytes;
        for (index, artifact) in plan.artifacts().iter().enumerate() {
            if preflight_stopped {
                continue;
            }
            match resumed_artifact(
                &layout,
                artifact,
                resume,
                &mut remaining_existing_hash_bytes,
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
                        &mut remaining_existing_hash_bytes,
                        &self.observer,
                    );
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
                    let fallback_target = artifact.fallback_path().map(|path| {
                        prepare_target(
                            &layout,
                            path,
                            options.existing_output,
                            resume_evidence_for_slot(resume_evidence, PlannedOutputSlot::Fallback),
                            &mut remaining_existing_hash_bytes,
                            &self.observer,
                        )
                    });
                    pending.push(PendingWork {
                        artifact_index: index,
                        working_set_bytes: working_sets[index],
                        preferred_target,
                        fallback_target,
                    });
                }
            }
        }

        let mut publication = PublicationState::new(outcomes.len(), artifact_output_limit);
        let mut pending_cursor = 0;
        let mut publish_cursor = 0;
        while pending_cursor < pending.len() && !publication.stopped {
            let batch = PendingBatch::select(options.limits, &pending, pending_cursor)?;
            let batch_end = batch.end;
            debug_assert!(batch.working_set_bytes <= options.limits.max_in_flight_bytes);
            debug_assert!(batch.open_files <= options.limits.max_open_files);
            let remaining_output = publication.remaining_output();
            let results = execute_pending_batch(
                view,
                plan,
                &layout,
                options,
                budget,
                &pending[pending_cursor..batch_end],
                remaining_output,
                &self.observer,
            );
            for (work, outcome) in pending[pending_cursor..batch_end].iter().zip(results) {
                outcomes[work.artifact_index] = Some(outcome);
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
        let report = publication.finish(plan)?;
        validate_actual_report(&report, options.limits.max_report_bytes)?;
        if let Some(manifest_path) = manifest_path {
            publish_manifest(
                &layout,
                manifest_path,
                &report,
                manifest_output_reservation,
                &self.observer,
            )?;
        }
        Ok(report)
    }
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
    #[error(
        "artifact {ordinal} streamed range {offset}..{end} exceeds source {locator:?} length {source_len}"
    )]
    StreamOutOfRange {
        ordinal: u32,
        locator: SourceLocator,
        offset: u64,
        end: u64,
        source_len: u64,
    },
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
    #[error("failed to reserve {requested} bytes for {resource}")]
    Allocation {
        resource: &'static str,
        requested: usize,
    },
    #[error("an extraction worker panicked while processing ordinal {ordinal}")]
    WorkerPanicked { ordinal: u32 },
    #[error("an extraction worker did not return an outcome for ordinal {ordinal}")]
    MissingWorkerOutcome { ordinal: u32 },
    #[error("failed to serialize the bounded report: {0}")]
    ReportSerialization(String),
    #[error("extraction output byte accounting overflowed")]
    OutputLengthOverflow,
}

impl ExtractionExecutionError {
    fn output_layout(error: OutputArtifactError) -> Self {
        Self::OutputLayout {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

#[derive(Debug)]
struct PendingWork {
    artifact_index: usize,
    working_set_bytes: u64,
    preferred_target: PreparedTarget,
    fallback_target: Option<PreparedTarget>,
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

#[cfg(feature = "decode")]
struct BatchCache<Key, Value> {
    entries: Mutex<Vec<(Key, Option<Arc<Value>>)>>,
}

#[cfg(feature = "decode")]
impl<Key, Value> Default for BatchCache<Key, Value> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(feature = "decode")]
impl<Key: Eq, Value> BatchCache<Key, Value> {
    fn get_or_try_insert_with(
        &self,
        key: Key,
        prepare: impl FnOnce() -> Result<Option<Value>, ExtractionExecutionError>,
    ) -> Result<Option<Arc<Value>>, ExtractionExecutionError> {
        let mut entries = lock_recover(&self.entries);
        if let Some((_, value)) = entries.iter().find(|(candidate, _)| candidate == &key) {
            return Ok(value.clone());
        }
        let value = prepare()?.map(Arc::new);
        entries
            .try_reserve(1)
            .map_err(|_| ExtractionExecutionError::Allocation {
                resource: "sprite texture cache",
                requested: std::mem::size_of::<(Key, Option<Arc<Value>>)>(),
            })?;
        entries.push((key, value.clone()));
        Ok(value)
    }
}

#[cfg(feature = "decode")]
#[derive(Clone, PartialEq, Eq)]
struct SpriteTextureKey {
    address: ObjectAddress,
    stream: Option<ExtractionSourceRange>,
}

#[cfg(feature = "decode")]
type SpriteTextureCache = BatchCache<SpriteTextureKey, DecodedSpriteTexture>;

#[cfg(not(feature = "decode"))]
#[derive(Default)]
struct SpriteTextureCache {
    _private: (),
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
    HashLimitExceeded,
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
    view: &dyn WorkspaceView,
    plan: &ExtractionPlan,
    limits: ExtractionExecutionLimits,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u64>, ExtractionExecutionError> {
    #[cfg(feature = "decode")]
    let stream_sources = plan
        .artifacts()
        .iter()
        .any(requires_stream_resolution)
        .then(|| view.sources(budget))
        .transpose()?;
    #[cfg(feature = "decode")]
    let stream_resolver = stream_sources
        .as_ref()
        .map(|sources| StreamedResourceResolver::new(view, sources, budget))
        .transpose()?;
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
        #[cfg(feature = "decode")]
        let required = trusted_working_set(view, artifact, stream_resolver.as_ref(), budget);
        #[cfg(not(feature = "decode"))]
        let required = trusted_working_set(view, artifact, budget);
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
fn map_reference_graph_error(ordinal: u32, error: ReferenceGraphError) -> ExtractionExecutionError {
    match error {
        ReferenceGraphError::Budget(error)
        | ReferenceGraphError::Workspace(WorkspaceError::Budget(error)) => error.into(),
        ReferenceGraphError::Workspace(error) => error.into(),
        ReferenceGraphError::Allocation {
            resource,
            requested,
            ..
        } => ExtractionExecutionError::Allocation {
            resource,
            requested,
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
        #[cfg(feature = "decode")]
        ExtractionReservationError::StreamOutOfRange {
            locator,
            offset,
            end,
            source_len,
        } => ExtractionExecutionError::StreamOutOfRange {
            ordinal,
            locator,
            offset,
            end,
            source_len,
        },
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

fn publish_manifest(
    layout: &OutputLayout,
    path: &ExtractionPath,
    report: &ExtractionReport,
    output_limit: u64,
    _observer: &ExecutionObserver,
) -> Result<(), ExtractionExecutionError> {
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
    let result = {
        #[cfg(all(test, feature = "decode"))]
        let _publish_open_files =
            _observer.reserve_open_files(SERIAL_OPEN_FILE_PEAK - EXECUTION_LOCK_OPEN_FILES);
        staged.publish(true)
    };
    result.map_err(ExtractionExecutionError::output_layout)
}

fn resumed_artifact(
    layout: &OutputLayout,
    artifact: &PlannedArtifact,
    resume: Option<&ExtractionManifest>,
    remaining_hash_bytes: &mut u64,
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
        match hash_existing_bounded(output, remaining_hash_bytes, observer) {
            Ok(Some(actual)) if actual == (expected_length, expected_digest) => {
                let diagnostics = artifact_diagnostics_with(
                    artifact,
                    artifact
                        .fallback_path()
                        .filter(|path| *path == candidate.path())
                        .map(|_| ExtractionDiagnosticCode::DecodeFailedRawFallback),
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
            Err(OutputArtifactError::ExistingHashLimitExceeded { .. }) => {
                return Ok(ResumeDecision::Execute(Some(ResumeEvidence {
                    slot,
                    existing: ExistingEvidence::HashLimitExceeded,
                })));
            }
            Err(error) => return Err(ExtractionExecutionError::output_layout(error)),
        }
    }
    Ok(ResumeDecision::Execute(None))
}

fn prepare_target(
    layout: &OutputLayout,
    path: &ExtractionPath,
    existing: ExistingOutputPolicy,
    evidence: Option<ExistingEvidence>,
    remaining_hash_bytes: &mut u64,
    observer: &ExecutionObserver,
) -> PreparedTarget {
    if let Some(evidence) = evidence {
        return target_from_existing_evidence(existing, evidence);
    }
    let output = match layout.path(path.as_str()) {
        Ok(output) => output,
        Err(_) => return PreparedTarget::Failed(ExtractionDiagnosticCode::OutputFailed),
    };
    match existing {
        ExistingOutputPolicy::Error => match output.exists() {
            Ok(true) => PreparedTarget::Failed(ExtractionDiagnosticCode::OutputExists),
            Ok(false) => PreparedTarget::Encode { replace: false },
            Err(_) => PreparedTarget::Failed(ExtractionDiagnosticCode::OutputFailed),
        },
        ExistingOutputPolicy::Replace => match output.exists() {
            Ok(exists) => PreparedTarget::Encode { replace: exists },
            Err(_) => PreparedTarget::Failed(ExtractionDiagnosticCode::OutputFailed),
        },
        ExistingOutputPolicy::Skip => {
            match hash_existing_bounded(output, remaining_hash_bytes, observer) {
                Ok(Some((length, digest))) => PreparedTarget::Existing { length, digest },
                Ok(None) => PreparedTarget::Encode { replace: false },
                Err(OutputArtifactError::ExistingHashLimitExceeded { .. }) => {
                    PreparedTarget::Failed(ExtractionDiagnosticCode::OutputLimitExceeded)
                }
                Err(_) => PreparedTarget::Failed(ExtractionDiagnosticCode::OutputFailed),
            }
        }
    }
}

fn target_from_existing_evidence(
    policy: ExistingOutputPolicy,
    evidence: ExistingEvidence,
) -> PreparedTarget {
    match evidence {
        ExistingEvidence::Missing => PreparedTarget::Encode { replace: false },
        ExistingEvidence::Existing { length, digest } => match policy {
            ExistingOutputPolicy::Error => {
                PreparedTarget::Failed(ExtractionDiagnosticCode::OutputExists)
            }
            ExistingOutputPolicy::Replace => PreparedTarget::Encode { replace: true },
            ExistingOutputPolicy::Skip => PreparedTarget::Existing { length, digest },
        },
        ExistingEvidence::HashLimitExceeded => match policy {
            ExistingOutputPolicy::Error => {
                PreparedTarget::Failed(ExtractionDiagnosticCode::OutputExists)
            }
            ExistingOutputPolicy::Replace => PreparedTarget::Encode { replace: true },
            ExistingOutputPolicy::Skip => {
                PreparedTarget::Failed(ExtractionDiagnosticCode::OutputLimitExceeded)
            }
        },
    }
}

fn hash_existing_bounded(
    output: &super::artifact::PreparedOutputPath,
    remaining: &mut u64,
    _observer: &ExecutionObserver,
) -> Result<Option<(u64, DigestV1)>, OutputArtifactError> {
    let result = output.hash_existing_bounded(*remaining)?;
    if let Some((length, _)) = result {
        *remaining = (*remaining)
            .checked_sub(length)
            .expect("bounded existing-output hash exceeded its remaining allowance");
    }
    #[cfg(all(test, feature = "decode"))]
    if let Some((length, _)) = result {
        _observer.record_existing_hash(length);
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
    view: &dyn WorkspaceView,
    plan: &ExtractionPlan,
    layout: &OutputLayout,
    options: &ExtractionExecutionOptions,
    budget: &mut AssetLoadBudget,
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
    let sprite_textures = SpriteTextureCache::default();

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
                            view,
                            artifact,
                            work,
                            layout,
                            pending_index,
                            &budget,
                            &preparation,
                            &sprite_textures,
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
    view: &dyn WorkspaceView,
    artifact: &PlannedArtifact,
    work: &PendingWork,
    layout: &OutputLayout,
    pending_index: usize,
    budget: &Mutex<&mut AssetLoadBudget>,
    preparation: &PreparationOrder,
    sprite_textures: &SpriteTextureCache,
    output_limit: u64,
    observer: &ExecutionObserver,
) -> WorkOutcome {
    #[cfg(all(test, feature = "decode"))]
    let _active_work = observer.enter_work(artifact.ordinal(), work.working_set_bytes);
    let prepared = preparation.run(pending_index, || {
        let mut budget = lock_recover(budget);
        let input = prepare_input(view, artifact, &mut budget, sprite_textures)?;
        if matches!(artifact.preferred_content(), PlannedContent::Yaml) {
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
    Input(PreparedInput),
    Complete(WorkOutcome),
}

struct PreparedInput {
    object: WorkspaceObject,
    #[cfg(feature = "decode")]
    stream: Option<Vec<u8>>,
    #[cfg(feature = "decode")]
    sprite_texture: Option<Arc<DecodedSpriteTexture>>,
    #[cfg(feature = "decode")]
    audio: Option<PreparedAudioSource>,
    #[cfg(feature = "decode")]
    embedded_audio: Option<Vec<u8>>,
}

fn prepare_input(
    view: &dyn WorkspaceView,
    artifact: &PlannedArtifact,
    budget: &mut AssetLoadBudget,
    _sprite_textures: &SpriteTextureCache,
) -> Result<PreparedInput, ExtractionExecutionError> {
    let object = read_object(view, artifact.address(), budget)?;
    let input = PreparedInput {
        object,
        #[cfg(feature = "decode")]
        stream: None,
        #[cfg(feature = "decode")]
        sprite_texture: None,
        #[cfg(feature = "decode")]
        audio: None,
        #[cfg(feature = "decode")]
        embedded_audio: None,
    };
    #[cfg(feature = "decode")]
    let input = {
        let mut input = input;
        match artifact.preferred_content() {
            PlannedContent::Audio {
                version, stream, ..
            } => {
                input.stream = stream
                    .as_ref()
                    .map(|range| read_range(view, range, budget))
                    .transpose()?;
                let WorkspaceObjectValue::Binary(object) = input.object.value() else {
                    return Ok(input);
                };
                let clip = match AudioClipConverter::new(version.clone()).from_unity_object(object)
                {
                    Ok(clip) => clip,
                    Err(_) => return Ok(input),
                };
                let bytes = if stream.is_some() {
                    match input.stream.as_deref() {
                        Some(bytes) => bytes,
                        None => return Ok(input),
                    }
                } else {
                    clip.data.as_slice()
                };
                let prepared = match AudioExporter::prepare_standard_source(&clip, bytes, budget) {
                    Ok(prepared) => Some(prepared),
                    Err(AudioSourceError::Budget(error)) => return Err(error.into()),
                    Err(
                        AudioSourceError::InvalidData(_)
                        | AudioSourceError::UnsupportedFormat(_)
                        | AudioSourceError::SourceChanged
                        | AudioSourceError::Output(_),
                    ) => None,
                };
                if prepared.is_some() && stream.is_none() {
                    input.embedded_audio = Some(clip.data);
                }
                input.audio = prepared;
            }
            PlannedContent::TexturePng { stream, .. } => {
                input.stream = stream
                    .as_ref()
                    .map(|range| read_range(view, range, budget))
                    .transpose()?;
            }
            PlannedContent::SpritePng {
                texture,
                texture_stream,
            } => {
                input.sprite_texture = prepare_sprite_texture(
                    view,
                    texture,
                    texture_stream,
                    budget,
                    _sprite_textures,
                )?;
            }
            PlannedContent::RawBinary | PlannedContent::Yaml | PlannedContent::TextAsset => {}
        }
        input
    };
    Ok(input)
}

#[cfg(feature = "decode")]
fn prepare_sprite_texture(
    view: &dyn WorkspaceView,
    address: &ObjectAddress,
    stream: &Option<ExtractionSourceRange>,
    budget: &mut AssetLoadBudget,
    cache: &SpriteTextureCache,
) -> Result<Option<Arc<DecodedSpriteTexture>>, ExtractionExecutionError> {
    let key = SpriteTextureKey {
        address: address.clone(),
        stream: stream.clone(),
    };
    cache.get_or_try_insert_with(key, || {
        let texture_object = read_object(view, address, budget)?;
        let WorkspaceObjectValue::Binary(texture_object) = texture_object.value() else {
            return Ok(None);
        };
        let processor = TextureProcessor::new();
        let Ok(mut texture) = processor.convert_object(texture_object) else {
            return Ok(None);
        };
        if let Some(stream) = stream {
            let bytes = read_range(view, stream, budget)?;
            let Ok(data_size) = i32::try_from(bytes.len()) else {
                return Ok(None);
            };
            texture.image_data = bytes;
            texture.data_size = data_size;
        }
        Ok(SpriteProcessor::new().decode_sprite_texture(&texture).ok())
    })
}

fn read_object(
    view: &dyn WorkspaceView,
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceObject, ExtractionExecutionError> {
    let handle = match view.resolve_object(address, budget)? {
        WorkspaceLookup::Resolved(handle) => handle,
        WorkspaceLookup::Unloaded
        | WorkspaceLookup::Missing
        | WorkspaceLookup::Ambiguous { .. }
        | WorkspaceLookup::Invalid { .. } => {
            return Err(WorkspaceError::MissingObject(Box::new(address.clone())).into());
        }
    };
    Ok(view.read_object(&handle, budget)?)
}

#[cfg(feature = "decode")]
fn read_range(
    view: &dyn WorkspaceView,
    range: &ExtractionSourceRange,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, ExtractionExecutionError> {
    let source = match view.resolve_source(range.source(), budget)? {
        WorkspaceLookup::Resolved(source) => source,
        WorkspaceLookup::Unloaded
        | WorkspaceLookup::Missing
        | WorkspaceLookup::Ambiguous { .. }
        | WorkspaceLookup::Invalid { .. } => {
            return Err(ExtractionExecutionError::SourceChanged {
                locator: range.source().clone(),
            });
        }
    };
    budget.consume_bytes(range.size())?;
    let size = usize::try_from(range.size()).map_err(|_| ExtractionExecutionError::Allocation {
        resource: "extraction streamed resource",
        requested: usize::MAX,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| ExtractionExecutionError::Allocation {
            resource: "extraction streamed resource",
            requested: size,
        })?;
    bytes.resize(size, 0);
    let source_range = view.read_source_range(source.id(), range.offset(), range.size(), budget)?;
    let mut reader = source_range.reader();
    io::Read::read_exact(&mut reader, &mut bytes).map_err(|_| {
        ExtractionExecutionError::SourceChanged {
            locator: range.source().clone(),
        }
    })?;
    Ok(bytes)
}

fn encode_artifact(
    artifact: &PlannedArtifact,
    work: &PendingWork,
    mut input: PreparedInput,
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
        artifact.preferred_content(),
        &mut input,
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
        Err(AttemptError::Decode) => {
            let (Some(kind), Some(path), Some(content)) = (
                artifact.fallback_kind(),
                artifact.fallback_path(),
                artifact.fallback_content(),
            ) else {
                return failed_outcome(artifact, ExtractionDiagnosticCode::DecodedUnavailable);
            };
            let diagnostics = artifact_diagnostics_with(
                artifact,
                Some(ExtractionDiagnosticCode::DecodeFailedRawFallback),
            );
            let target = work
                .fallback_target
                .expect("a planned fallback has a preflighted target");
            let replace = match target {
                PreparedTarget::Encode { replace } => replace,
                target => {
                    return prepared_target_outcome(artifact, kind, path, diagnostics, target)
                        .expect("only an encoding target continues fallback extraction");
                }
            };
            match stage_content(
                layout,
                path,
                content,
                &mut input,
                artifact.ordinal(),
                output_limit,
                None,
                observer,
            ) {
                Ok(staged) => WorkOutcome::Staged {
                    kind,
                    path: path.clone(),
                    staged,
                    replace,
                    diagnostics,
                },
                Err(AttemptError::Fatal(error)) => WorkOutcome::Fatal(*error),
                Err(AttemptError::Decode | AttemptError::Output) => {
                    failed_outcome(artifact, ExtractionDiagnosticCode::OutputFailed)
                }
                Err(AttemptError::OutputLimit) => {
                    failed_outcome(artifact, ExtractionDiagnosticCode::OutputLimitExceeded)
                }
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

fn stage_content(
    layout: &OutputLayout,
    path: &ExtractionPath,
    content: &PlannedContent,
    input: &mut PreparedInput,
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
            write_content(&mut writer, content, input, budget)
        };
        #[cfg(not(all(test, feature = "decode")))]
        let result = write_content(&mut writer, content, input, budget);
        (result, writer.exceeded())
    };
    if exceeded {
        return Err(AttemptError::OutputLimit);
    }
    result?;
    staging.finish().map_err(|_| AttemptError::Output)
}

fn write_content(
    writer: &mut dyn Write,
    content: &PlannedContent,
    input: &mut PreparedInput,
    budget: Option<&mut AssetLoadBudget>,
) -> Result<(), AttemptError> {
    match content {
        PlannedContent::RawBinary => {
            let WorkspaceObjectValue::Binary(object) = input.object.value() else {
                return Err(AttemptError::Decode);
            };
            writer
                .write_all(object.raw_data())
                .map_err(|_| AttemptError::Output)
        }
        PlannedContent::Yaml => {
            let budget = budget.ok_or(AttemptError::Decode)?;
            let result = UnityYamlSerializer::new().serialize_to_writer_with_budget(
                writer,
                std::iter::once(input.object.class()),
                budget,
            );
            match result {
                Ok(()) => Ok(()),
                Err(error) => match source_budget_error(&error) {
                    Some(error) => Err(AttemptError::Fatal(Box::new(error.clone().into()))),
                    None => Err(AttemptError::Output),
                },
            }
        }
        PlannedContent::TextAsset => write_text_asset(writer, &input.object),
        #[cfg(feature = "decode")]
        PlannedContent::Audio {
            version: _,
            extension: _,
            stream,
        } => write_audio(writer, stream, input),
        #[cfg(feature = "decode")]
        PlannedContent::TexturePng { version: _, stream } => write_texture(writer, stream, input),
        #[cfg(feature = "decode")]
        PlannedContent::SpritePng { .. } => write_sprite(writer, input),
        #[cfg(not(feature = "decode"))]
        PlannedContent::Audio { .. }
        | PlannedContent::TexturePng { .. }
        | PlannedContent::SpritePng { .. } => Err(AttemptError::Decode),
    }
}

fn write_text_asset(writer: &mut dyn Write, object: &WorkspaceObject) -> Result<(), AttemptError> {
    let WorkspaceObjectValue::Binary(object) = object.value() else {
        return Err(AttemptError::Decode);
    };
    for key in ["m_Script", "m_Text", "m_Bytes", "m_Data"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        return match value {
            UnityValue::String(value) => writer
                .write_all(value.as_bytes())
                .map_err(|_| AttemptError::Output),
            UnityValue::Bytes(value) => writer.write_all(value).map_err(|_| AttemptError::Output),
            UnityValue::Array(values) => write_byte_array(writer, values),
            _ => Err(AttemptError::Decode),
        };
    }
    Err(AttemptError::Decode)
}

fn write_byte_array(writer: &mut dyn Write, values: &[UnityValue]) -> Result<(), AttemptError> {
    let mut buffer = [0_u8; 8192];
    for chunk in values.chunks(buffer.len()) {
        for (output, value) in buffer.iter_mut().zip(chunk) {
            *output = value
                .as_u64()
                .and_then(|value| u8::try_from(value).ok())
                .ok_or(AttemptError::Decode)?;
        }
        writer
            .write_all(&buffer[..chunk.len()])
            .map_err(|_| AttemptError::Output)?;
    }
    Ok(())
}

#[cfg(feature = "decode")]
fn write_audio(
    writer: &mut dyn Write,
    stream: &Option<ExtractionSourceRange>,
    input: &PreparedInput,
) -> Result<(), AttemptError> {
    let prepared = input.audio.as_ref().ok_or(AttemptError::Decode)?;
    let bytes = if stream.is_some() {
        input.stream.as_deref().ok_or(AttemptError::Decode)?
    } else {
        input
            .embedded_audio
            .as_deref()
            .ok_or(AttemptError::Decode)?
    };
    prepared
        .write_to(bytes, writer)
        .map_err(|error| match error {
            AudioSourceError::Output(_) => AttemptError::Output,
            AudioSourceError::Budget(error) => AttemptError::Fatal(Box::new(error.into())),
            AudioSourceError::InvalidData(_)
            | AudioSourceError::UnsupportedFormat(_)
            | AudioSourceError::SourceChanged => AttemptError::Decode,
        })
}

#[cfg(feature = "decode")]
fn write_texture(
    writer: &mut dyn Write,
    stream: &Option<ExtractionSourceRange>,
    input: &mut PreparedInput,
) -> Result<(), AttemptError> {
    let WorkspaceObjectValue::Binary(object) = input.object.value() else {
        return Err(AttemptError::Decode);
    };
    let processor = TextureProcessor::new();
    let mut texture = processor
        .convert_object(object)
        .map_err(|_| AttemptError::Decode)?;
    if stream.is_some() {
        texture.image_data = input.stream.take().ok_or(AttemptError::Decode)?;
        texture.data_size =
            i32::try_from(texture.image_data.len()).map_err(|_| AttemptError::Decode)?;
    }
    let image = processor
        .decode_texture(&texture)
        .map_err(|_| AttemptError::Decode)?;
    png_output_result(TextureExporter::write_png(&image, writer))
}

#[cfg(feature = "decode")]
fn write_sprite(writer: &mut dyn Write, input: &mut PreparedInput) -> Result<(), AttemptError> {
    let WorkspaceObjectValue::Binary(sprite_object) = input.object.value() else {
        return Err(AttemptError::Decode);
    };
    let texture = input
        .sprite_texture
        .as_deref()
        .ok_or(AttemptError::Decode)?;
    let sprite_processor = SpriteProcessor::new();
    let sprite = sprite_processor
        .parse_sprite(sprite_object)
        .map_err(|_| AttemptError::Decode)?;
    let image = sprite_processor
        .render_sprite_from_texture(&sprite, texture)
        .map_err(|_| AttemptError::Decode)?;
    png_output_result(TextureExporter::write_png(&image, writer))
}

#[cfg(feature = "decode")]
fn png_output_result<T>(result: unity_asset_decode::Result<T>) -> Result<T, AttemptError> {
    result.map_err(|error| match error {
        unity_asset_decode::BinaryError::Io(_) => AttemptError::Output,
        unity_asset_decode::BinaryError::Budget(error) => {
            AttemptError::Fatal(Box::new(error.into()))
        }
        _ => AttemptError::Decode,
    })
}

enum AttemptError {
    Decode,
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

struct PublicationState {
    receipts: Vec<ExtractionManifestArtifact>,
    published_bytes: u64,
    output_limit: u64,
    stopped: bool,
}

impl PublicationState {
    fn new(capacity: usize, output_limit: u64) -> Self {
        Self {
            receipts: Vec::with_capacity(capacity),
            published_bytes: 0,
            output_limit,
            stopped: false,
        }
    }

    const fn remaining_output(&self) -> u64 {
        self.output_limit.saturating_sub(self.published_bytes)
    }

    fn finish(self, plan: &ExtractionPlan) -> Result<ExtractionReport, ExtractionExecutionError> {
        let manifest = ExtractionManifest::new(plan, self.receipts)?;
        Ok(ExtractionReport::new(manifest)?)
    }
}

fn publish_ready(
    plan: &ExtractionPlan,
    options: &ExtractionExecutionOptions,
    outcomes: &mut [Option<WorkOutcome>],
    artifact_offset: usize,
    state: &mut PublicationState,
    _observer: &ExecutionObserver,
) -> Result<(), ExtractionExecutionError> {
    for (local_index, outcome) in outcomes.iter_mut().enumerate() {
        let artifact_index = artifact_offset + local_index;
        let artifact = &plan.artifacts()[artifact_index];
        if state.stopped {
            if let Some(WorkOutcome::Staged { staged, .. }) = outcome.take() {
                let _ = staged.discard();
            }
            state.receipts.push(stopped_receipt(artifact)?);
            continue;
        }

        let outcome = outcome
            .take()
            .ok_or(ExtractionExecutionError::MissingWorkerOutcome {
                ordinal: artifact.ordinal(),
            })?;
        match outcome {
            WorkOutcome::Receipt(receipt) => {
                let failed = receipt.status() == ExtractionArtifactStatus::Failed;
                state.receipts.push(receipt);
                if failed && options.failure == ExtractionFailurePolicy::StopInPlanOrder {
                    state.stopped = true;
                }
            }
            WorkOutcome::Staged {
                kind,
                path,
                staged,
                replace,
                diagnostics,
            } => {
                let next = state.published_bytes.checked_add(staged.length());
                if next.is_none_or(|next| next > state.output_limit) {
                    let _ = staged.discard();
                    state.receipts.push(failed_receipt(
                        artifact,
                        ExtractionDiagnosticCode::OutputLimitExceeded,
                    )?);
                    if options.failure == ExtractionFailurePolicy::StopInPlanOrder {
                        state.stopped = true;
                    }
                    continue;
                }
                let length = staged.length();
                let digest = staged.digest();
                let publish_result = {
                    #[cfg(all(test, feature = "decode"))]
                    let _open_files = _observer
                        .reserve_open_files(SERIAL_OPEN_FILE_PEAK - EXECUTION_LOCK_OPEN_FILES);
                    staged.publish(replace)
                };
                match publish_result {
                    Ok(()) => {
                        state.published_bytes =
                            next.ok_or(ExtractionExecutionError::OutputLengthOverflow)?;
                        state.receipts.push(ExtractionManifestArtifact::new(
                            artifact.ordinal(),
                            artifact.address().clone(),
                            kind,
                            path,
                            ExtractionArtifactStatus::Written,
                            Some(length),
                            Some(digest),
                            diagnostics,
                        )?);
                    }
                    Err(_) => {
                        state.receipts.push(failed_receipt(
                            artifact,
                            ExtractionDiagnosticCode::OutputFailed,
                        )?);
                        if options.failure == ExtractionFailurePolicy::StopInPlanOrder {
                            state.stopped = true;
                        }
                    }
                }
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
            fallback_target: None,
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
        let limits = ExtractionExecutionLimits::new(8, 10, 5, 1024, 1024).unwrap();

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
        let limits = ExtractionExecutionLimits::new(8, 1024, 7, 1024, 1024).unwrap();

        let batch = PendingBatch::select(limits, &pending, 0).unwrap();

        assert_eq!(batch.end, 3);
        assert_eq!(batch.working_set_bytes, 3);
        assert_eq!(batch.open_files, 7);
    }
}

#[cfg(all(test, feature = "decode"))]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::test_probe::ExecutionProbe;
    use super::*;
    use crate::extraction::{
        ExtractionFilter, ExtractionPlanner, ExtractionRepresentationPolicy, ExtractionRequest,
    };
    use crate::reference::ReferenceGraphBuildOptions;
    use crate::workspace::{AssetWorkspace, WorkspaceSnapshot};
    use unity_asset_binary::asset::class_ids;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected PNG sink failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn png_sink_errors_are_output_errors() {
        let image = image::RgbaImage::new(1, 1);
        let result = png_output_result(TextureExporter::write_png(&image, &mut FailingWriter));

        assert!(matches!(result, Err(AttemptError::Output)));
    }

    #[test]
    fn png_codec_errors_are_decode_errors() {
        let result = png_output_result::<()>(Err(unity_asset_decode::BinaryError::InvalidData(
            "invalid dimensions".to_owned(),
        )));

        assert!(matches!(result, Err(AttemptError::Decode)));
    }

    #[test]
    fn batch_cache_prepares_each_key_once_including_failures() {
        let cache = BatchCache::<u8, String>::default();
        let calls = Cell::new(0_u8);
        let first = cache
            .get_or_try_insert_with(1, || {
                calls.set(calls.get() + 1);
                Ok(Some("atlas".to_owned()))
            })
            .unwrap()
            .unwrap();
        let second = cache
            .get_or_try_insert_with(1, || {
                calls.set(calls.get() + 1);
                Ok(Some("duplicate".to_owned()))
            })
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        assert!(
            cache
                .get_or_try_insert_with(2, || {
                    calls.set(calls.get() + 1);
                    Ok(None)
                })
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .get_or_try_insert_with(2, || {
                    calls.set(calls.get() + 1);
                    Ok(Some("unexpected".to_owned()))
                })
                .unwrap()
                .is_none()
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn executor_rejects_tampered_stream_ranges_before_creating_output() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = media_plan(&directory);
        let mut wire = serde_json::to_value(&plan).unwrap();
        let (ordinal, stream) = wire["artifacts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find_map(|artifact| {
                artifact["preferred_content"]["stream"]
                    .is_object()
                    .then(|| {
                        (
                            u32::try_from(artifact["ordinal"].as_u64().unwrap()).unwrap(),
                            artifact["preferred_content"]["stream"]
                                .as_object_mut()
                                .unwrap(),
                        )
                    })
            })
            .expect("media fixture must include a streamed artifact");
        let offset = stream["offset"].as_u64().unwrap();
        stream.insert("offset".to_owned(), serde_json::Value::from(offset + 1));
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
            ExtractionExecutionError::WorkingSetProofFailed {
                ordinal: actual
            } if actual == ordinal
        ));
        assert!(!output.exists());
    }

    #[test]
    fn executor_rejects_tampered_sprite_texture_proofs_before_creating_output() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan, sprite_index) = sprite_plan();
        let ordinal = plan.artifacts()[sprite_index].ordinal();

        let mut texture_wire = serde_json::to_value(&plan).unwrap();
        let texture = &mut texture_wire["artifacts"][sprite_index]["preferred_content"]["texture"];
        let path_id = texture["path_id"].as_i64().unwrap();
        texture["path_id"] = serde_json::Value::from(path_id.saturating_add(1));
        let texture_plan = ExtractionPlan::read_json(
            serde_json::to_vec(&texture_wire).unwrap().as_slice(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_working_set_proof_rejected(
            &snapshot,
            &texture_plan,
            &directory.path().join("tampered-texture"),
            ordinal,
        );
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
        assert_eq!(error_probe.snapshot().existing_hash_bytes, 0);

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
        assert_eq!(replace_probe.snapshot().existing_hash_bytes, 0);
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
        assert_eq!(resume_probe.snapshot().existing_hash_bytes, encoded_length);

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
        assert_eq!(skip_probe.snapshot().existing_hash_bytes, encoded_length);

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
            mismatch_probe.snapshot().existing_hash_bytes,
            encoded_length
        );

        fs::write(&output_path, b"too large").unwrap();
        let bounded_probe = ExecutionProbe::new([], []);
        let bounded = ExtractionExecutor::observing(Arc::clone(&bounded_probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options(&plan, 1, ExistingOutputPolicy::Skip)),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(bounded.counts().failed(), 1);
        assert_eq!(
            bounded.manifest().artifacts()[0].diagnostics()[0].code(),
            ExtractionDiagnosticCode::OutputLimitExceeded
        );
        assert_eq!(bounded_probe.snapshot().existing_hash_bytes, 0);
    }

    #[test]
    fn existing_output_hash_limit_is_cumulative_across_the_run() {
        let directory = tempfile::tempdir().unwrap();
        let (snapshot, plan) = media_plan(&directory);
        let output = directory.path().join("cumulative-existing-hash");
        for artifact in plan.artifacts() {
            write_existing(&output, artifact.preferred_path(), b"four");
        }
        let probe = ExecutionProbe::new([], []);

        let report = ExtractionExecutor::observing(Arc::clone(&probe))
            .execute(
                &snapshot,
                &plan,
                &output,
                ExtractionRunOptions::new(output_options(&plan, 7, ExistingOutputPolicy::Skip)),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        assert_eq!(
            report.manifest().artifacts()[0].status(),
            ExtractionArtifactStatus::SkippedExisting
        );
        assert_eq!(
            report.manifest().artifacts()[1].diagnostics()[0].code(),
            ExtractionDiagnosticCode::OutputLimitExceeded
        );
        assert_eq!(probe.snapshot().existing_hash_bytes, 4);
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

    fn sprite_plan() -> (WorkspaceSnapshot, ExtractionPlan, usize) {
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
        let graph = snapshot
            .reference_graph(
                ReferenceGraphBuildOptions::unbounded(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::PreferDecoded)
            .with_filter(ExtractionFilter::new([class_ids::SPRITE], None, None, None).unwrap());
        let plan = ExtractionPlanner::new(&snapshot)
            .with_reference_graph(&graph)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();
        let sprite_index = plan
            .artifacts()
            .iter()
            .position(|artifact| artifact.preferred_kind() == ExtractionArtifactKind::SpritePng)
            .expect("fixture must contain at least one resolvable Sprite texture");
        (snapshot, plan, sprite_index)
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

    fn assert_working_set_proof_rejected(
        snapshot: &WorkspaceSnapshot,
        plan: &ExtractionPlan,
        output: &Path,
        ordinal: u32,
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
            ExtractionExecutionError::WorkingSetProofFailed { ordinal: actual }
                if actual == ordinal
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
        let in_flight = plan
            .artifacts()
            .iter()
            .map(PlannedArtifact::working_set_bytes)
            .max()
            .unwrap();
        ExtractionExecutionOptions::new(
            ExtractionExecutionLimits::new(1, in_flight, 5, output_limit, 16 * 1024 * 1024)
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
