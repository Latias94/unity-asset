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
};
use unity_asset_yaml::UnityYamlSerializer;

#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipConverter, AudioExporter, AudioSourceError, PreparedAudioSource},
    sprite::{DecodedSpriteTexture, SpriteProcessor},
    texture::{TextureExporter, TextureProcessor},
};

use super::artifact::{OutputArtifactError, OutputLayout, StagedOutput};
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
use super::source_budget_error;
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

impl ExtractionExecutionLimits {
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
        Ok(())
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

/// Executes immutable extraction plans against their exact workspace revision.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtractionExecutor;

impl ExtractionExecutor {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Execute an immutable plan against the exact workspace revision.
    ///
    /// A resume manifest is verification evidence, not overwrite authority.
    /// Missing or mismatched outputs continue to obey `options.existing_output`.
    #[allow(clippy::too_many_arguments)]
    pub fn execute(
        &self,
        view: &dyn WorkspaceView,
        plan: &ExtractionPlan,
        output_root: &Path,
        options: &ExtractionExecutionOptions,
        resume: Option<&ExtractionManifest>,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionReport, ExtractionExecutionError> {
        self.execute_inner(view, plan, output_root, None, options, resume, budget)
    }

    /// Execute a plan and safely publish its canonical manifest under the output root.
    ///
    /// `manifest_path` is a validated relative path. It participates in the same
    /// no-follow layout and exclusive output lock as planned artifacts, so it
    /// cannot collide with them or escape the selected root. An explicit manifest
    /// destination replaces its previous contents only after artifact execution
    /// has produced a complete report.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_with_manifest(
        &self,
        view: &dyn WorkspaceView,
        plan: &ExtractionPlan,
        output_root: &Path,
        manifest_path: &ExtractionPath,
        options: &ExtractionExecutionOptions,
        resume: Option<&ExtractionManifest>,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionReport, ExtractionExecutionError> {
        self.execute_inner(
            view,
            plan,
            output_root,
            Some(manifest_path),
            options,
            resume,
            budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_inner(
        &self,
        view: &dyn WorkspaceView,
        plan: &ExtractionPlan,
        output_root: &Path,
        manifest_path: Option<&ExtractionPath>,
        options: &ExtractionExecutionOptions,
        resume: Option<&ExtractionManifest>,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionReport, ExtractionExecutionError> {
        options.limits.validate()?;
        validate_context(view, plan)?;
        validate_sources(view, plan, budget)?;
        validate_resume(plan, resume)?;
        validate_working_sets(plan, options.limits)?;
        validate_report_bound(plan, options.limits.max_report_bytes)?;

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

        let mut outcomes = (0..plan.artifacts().len())
            .map(|_| None)
            .collect::<Vec<Option<WorkOutcome>>>();
        let mut pending = Vec::new();
        for (index, artifact) in plan.artifacts().iter().enumerate() {
            match resumed_artifact(&layout, plan, artifact, resume)? {
                ResumeDecision::Complete(receipt) => {
                    outcomes[index] = Some(WorkOutcome::Receipt(receipt));
                }
                ResumeDecision::Execute => pending.push(PendingWork {
                    artifact_index: index,
                }),
            }
        }

        let mut publication = PublicationState::new(outcomes.len());
        let mut pending_cursor = 0;
        let mut publish_cursor = 0;
        while pending_cursor < pending.len() && !publication.stopped {
            let batch_end = pending_batch_end(plan, options.limits, &pending, pending_cursor)?;
            let remaining_output = options
                .limits
                .max_output_bytes
                .saturating_sub(publication.published_bytes);
            let results = execute_pending_batch(
                view,
                plan,
                &layout,
                options,
                budget,
                &pending[pending_cursor..batch_end],
                remaining_output,
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
        )?;
        let report = publication.finish(plan)?;
        validate_actual_report(&report, options.limits.max_report_bytes)?;
        if let Some(manifest_path) = manifest_path {
            publish_manifest(&layout, manifest_path, &report)?;
        }
        Ok(report)
    }
}

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
    #[error("canonical extraction report requires at most {required} bytes, limit is {limit}")]
    ReportLimitExceeded { required: u64, limit: u64 },
    #[error("failed to prepare the safe output layout: {message}")]
    OutputLayout { message: String },
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
            message: error.to_string(),
        }
    }
}

#[derive(Debug)]
struct PendingWork {
    artifact_index: usize,
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
    version: unity_asset_binary::unity_version::UnityVersion,
    stream: Option<ExtractionSourceRange>,
}

#[cfg(feature = "decode")]
type SpriteTextureCache = BatchCache<SpriteTextureKey, DecodedSpriteTexture>;

#[cfg(not(feature = "decode"))]
#[derive(Default)]
struct SpriteTextureCache {
    _private: (),
}

fn pending_batch_end(
    plan: &ExtractionPlan,
    limits: ExtractionExecutionLimits,
    pending: &[PendingWork],
    start: usize,
) -> Result<usize, ExtractionExecutionError> {
    let maximum_count = limits.workers.min(limits.max_open_files);
    let mut end = start;
    let mut bytes = 0_u64;
    while let Some(work) = pending.get(end) {
        if end - start == maximum_count {
            break;
        }
        let artifact = &plan.artifacts()[work.artifact_index];
        let weight = artifact.working_set_bytes().max(1);
        let Some(next) = bytes.checked_add(weight) else {
            return Err(ExtractionExecutionError::OutputLengthOverflow);
        };
        if end > start && next > limits.max_in_flight_bytes {
            break;
        }
        bytes = next;
        end += 1;
    }
    Ok(end)
}

enum ResumeDecision {
    Complete(ExtractionManifestArtifact),
    Execute,
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

fn validate_working_sets(
    plan: &ExtractionPlan,
    limits: ExtractionExecutionLimits,
) -> Result<(), ExtractionExecutionError> {
    if let Some(artifact) = plan
        .artifacts()
        .iter()
        .find(|artifact| artifact.working_set_bytes().max(1) > limits.max_in_flight_bytes)
    {
        return Err(ExtractionExecutionError::WorkingSetExceedsLimit {
            ordinal: artifact.ordinal(),
            required: artifact.working_set_bytes().max(1),
            limit: limits.max_in_flight_bytes,
        });
    }
    Ok(())
}

fn validate_report_bound(
    plan: &ExtractionPlan,
    limit: u64,
) -> Result<(), ExtractionExecutionError> {
    let bound = maximum_extraction_report(plan)?;
    let required = canonical_length(&bound)?;
    if required > limit {
        return Err(ExtractionExecutionError::ReportLimitExceeded { required, limit });
    }
    Ok(())
}

fn publish_manifest(
    layout: &OutputLayout,
    path: &ExtractionPath,
    report: &ExtractionReport,
) -> Result<(), ExtractionExecutionError> {
    let output = layout
        .path(path.as_str())
        .map_err(ExtractionExecutionError::output_layout)?;
    let mut staging = output
        .create_staging()
        .map_err(ExtractionExecutionError::output_layout)?;
    report.write_canonical_manifest_json(staging.writer())?;
    let staged = staging
        .finish()
        .map_err(ExtractionExecutionError::output_layout)?;
    staged
        .publish(true)
        .map_err(ExtractionExecutionError::output_layout)
}

fn resumed_artifact(
    layout: &OutputLayout,
    _plan: &ExtractionPlan,
    artifact: &PlannedArtifact,
    resume: Option<&ExtractionManifest>,
) -> Result<ResumeDecision, ExtractionExecutionError> {
    let Some(candidate) =
        resume.and_then(|manifest| manifest.artifact_by_ordinal(artifact.ordinal()))
    else {
        return Ok(ResumeDecision::Execute);
    };
    let resumable_status = matches!(
        candidate.status(),
        ExtractionArtifactStatus::Written | ExtractionArtifactStatus::Resumed
    );
    let planned = artifact.address() == candidate.address()
        && artifact.matches_output(candidate.kind(), candidate.path());
    let evidence = candidate.length().zip(candidate.digest());
    if resumable_status
        && planned
        && let Some((expected_length, expected_digest)) = evidence
    {
        let output = layout
            .path(candidate.path().as_str())
            .map_err(ExtractionExecutionError::output_layout)?;
        if output
            .hash_existing()
            .map_err(ExtractionExecutionError::output_layout)?
            == Some((expected_length, expected_digest))
        {
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
    }
    Ok(ResumeDecision::Execute)
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
                            layout,
                            options.existing_output,
                            pending_index,
                            &budget,
                            &preparation,
                            &sprite_textures,
                            output_limit,
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
    layout: &OutputLayout,
    existing: ExistingOutputPolicy,
    pending_index: usize,
    budget: &Mutex<&mut AssetLoadBudget>,
    preparation: &PreparationOrder,
    sprite_textures: &SpriteTextureCache,
    output_limit: u64,
) -> WorkOutcome {
    let prepared = preparation.run(pending_index, || {
        let mut budget = lock_recover(budget);
        let input = prepare_input(view, artifact, &mut budget, sprite_textures)?;
        if matches!(artifact.preferred_content(), PlannedContent::Yaml) {
            return Ok(PreparedWork::Complete(encode_artifact(
                artifact,
                input,
                layout,
                existing,
                output_limit.min(artifact.working_set_bytes().max(1)),
                Some(&mut budget),
            )));
        }
        Ok(PreparedWork::Input(input))
    });
    match prepared {
        Err(error) => WorkOutcome::Fatal(error),
        Ok(PreparedWork::Complete(outcome)) => outcome,
        Ok(PreparedWork::Input(input)) => encode_artifact(
            artifact,
            input,
            layout,
            existing,
            output_limit.min(artifact.working_set_bytes().max(1)),
            None,
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
                version,
                texture,
                texture_stream,
            } => {
                input.sprite_texture = prepare_sprite_texture(
                    view,
                    version,
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
    version: &unity_asset_binary::unity_version::UnityVersion,
    address: &ObjectAddress,
    stream: &Option<ExtractionSourceRange>,
    budget: &mut AssetLoadBudget,
    cache: &SpriteTextureCache,
) -> Result<Option<Arc<DecodedSpriteTexture>>, ExtractionExecutionError> {
    let key = SpriteTextureKey {
        address: address.clone(),
        version: version.clone(),
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
        Ok(SpriteProcessor::new(version.clone())
            .decode_sprite_texture(&texture)
            .ok())
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
    mut input: PreparedInput,
    layout: &OutputLayout,
    existing: ExistingOutputPolicy,
    output_limit: u64,
    budget: Option<&mut AssetLoadBudget>,
) -> WorkOutcome {
    let planned_diagnostics = artifact.diagnostics().to_vec();
    let preferred_replace = match target_decision(
        artifact,
        artifact.preferred_kind(),
        artifact.preferred_path(),
        &planned_diagnostics,
        layout,
        existing,
    ) {
        TargetDecision::Encode { replace } => replace,
        TargetDecision::Complete(outcome) => return *outcome,
    };
    let preferred = stage_content(
        layout,
        artifact.preferred_path(),
        artifact.preferred_content(),
        &mut input,
        output_limit,
        budget,
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
            let replace =
                match target_decision(artifact, kind, path, &diagnostics, layout, existing) {
                    TargetDecision::Encode { replace } => replace,
                    TargetDecision::Complete(outcome) => return *outcome,
                };
            match stage_content(layout, path, content, &mut input, output_limit, None) {
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

enum TargetDecision {
    Encode { replace: bool },
    Complete(Box<WorkOutcome>),
}

fn target_decision(
    artifact: &PlannedArtifact,
    kind: ExtractionArtifactKind,
    path: &ExtractionPath,
    diagnostics: &[ExtractionDiagnostic],
    layout: &OutputLayout,
    existing: ExistingOutputPolicy,
) -> TargetDecision {
    let output = match layout.path(path.as_str()) {
        Ok(output) => output,
        Err(_) => {
            return TargetDecision::Complete(Box::new(failed_outcome(
                artifact,
                ExtractionDiagnosticCode::OutputFailed,
            )));
        }
    };
    match output.hash_existing() {
        Ok(Some((length, digest))) => match existing {
            ExistingOutputPolicy::Error => TargetDecision::Complete(Box::new(failed_outcome(
                artifact,
                ExtractionDiagnosticCode::OutputExists,
            ))),
            ExistingOutputPolicy::Skip => TargetDecision::Complete(Box::new(receipt_outcome(
                artifact,
                kind,
                path,
                ExtractionArtifactStatus::SkippedExisting,
                Some(length),
                Some(digest),
                diagnostics.to_vec(),
            ))),
            ExistingOutputPolicy::Replace => TargetDecision::Encode { replace: true },
        },
        Ok(None) => TargetDecision::Encode { replace: false },
        Err(_) => TargetDecision::Complete(Box::new(failed_outcome(
            artifact,
            ExtractionDiagnosticCode::OutputFailed,
        ))),
    }
}

fn stage_content(
    layout: &OutputLayout,
    path: &ExtractionPath,
    content: &PlannedContent,
    input: &mut PreparedInput,
    output_limit: u64,
    budget: Option<&mut AssetLoadBudget>,
) -> Result<StagedOutput, AttemptError> {
    let output = layout
        .path(path.as_str())
        .map_err(|_| AttemptError::Output)?;
    let mut staging = output.create_staging().map_err(|_| AttemptError::Output)?;
    let (result, exceeded) = {
        let mut writer = OutputLimitWriter::new(staging.writer(), output_limit);
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
        PlannedContent::SpritePng {
            version,
            texture: _,
            texture_stream: _,
        } => write_sprite(writer, version, input),
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
fn write_sprite(
    writer: &mut dyn Write,
    version: &unity_asset_binary::unity_version::UnityVersion,
    input: &mut PreparedInput,
) -> Result<(), AttemptError> {
    let WorkspaceObjectValue::Binary(sprite_object) = input.object.value() else {
        return Err(AttemptError::Decode);
    };
    let texture = input
        .sprite_texture
        .as_deref()
        .ok_or(AttemptError::Decode)?;
    let sprite_processor = SpriteProcessor::new(version.clone());
    let sprite = sprite_processor
        .parse_sprite(sprite_object)
        .map_err(|_| AttemptError::Decode)?
        .sprite;
    let image = sprite_processor
        .render_sprite_from_texture(&sprite, texture)
        .map_err(|_| AttemptError::Decode)?;
    png_output_result(TextureExporter::write_png(&image, writer))
}

#[cfg(feature = "decode")]
fn png_output_result<T>(result: unity_asset_decode::Result<T>) -> Result<T, AttemptError> {
    result.map_err(|_| AttemptError::Output)
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
    stopped: bool,
}

impl PublicationState {
    fn new(capacity: usize) -> Self {
        Self {
            receipts: Vec::with_capacity(capacity),
            published_bytes: 0,
            stopped: false,
        }
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
                if next.is_none_or(|next| next > options.limits.max_output_bytes) {
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
                match staged.publish(replace) {
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
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| ExtractionExecutionError::ReportSerialization(error.to_string()))?;
    Ok(counter.bytes)
}

#[derive(Default)]
struct ByteCounter {
    bytes: u64,
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

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("report length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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

#[cfg(all(test, feature = "decode"))]
mod tests {
    use std::cell::Cell;

    use super::*;

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
}
