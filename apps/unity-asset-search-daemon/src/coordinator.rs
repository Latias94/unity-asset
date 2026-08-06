//! Serial admission and execution for every daemon reindex trigger.
//!
//! Admission is intentionally centralized here. Filesystem events, timers, IPC requests, and
//! startup reconciliation must not grow independent scheduling rules around the index builder.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use futures::FutureExt as _;
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::Instant;
use unity_asset_search_index::{
    FilesystemReindexIntent, FilesystemReindexScope, ProjectPathSet, ProjectPathSpace,
};
use unity_asset_search_protocol::{
    ProjectId, ReindexDisposition, ReindexReceipt, SEARCH_PROTOCOL_REVISION, StatusResponse,
    ValidateContract,
};

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(75);
const DEFAULT_MAX_DEBOUNCE: Duration = Duration::from_millis(750);
const DEFAULT_MAX_DIRTY_PATHS: usize = 4_096;
const DEFAULT_MAX_PENDING_EVENTS: usize = 32_768;
const DEFAULT_MAX_FAILURE_HISTORY: usize = 64;
const MAX_FAILURE_MESSAGE_BYTES: usize = 4_096;
const REINDEX_INTENT_FINGERPRINT_DOMAIN: &[u8] = b"unity-asset:search-daemon:reindex-intent:v1\0";

type BuildFuture = Pin<Box<dyn Future<Output = anyhow::Result<ReindexExecution>> + Send + 'static>>;
type BuildExecutor = dyn Fn(FilesystemReindexIntent) -> BuildFuture + Send + Sync + 'static;
type CompletionOutcome = Result<ReindexExecution, ExecutionFailure>;
const OBSERVATION_EVENT_CAPACITY: usize = 3;

#[derive(Debug, Clone)]
enum ObservationEvent {
    Coalesced,
    Running,
    Completed(Arc<CompletionOutcome>),
    Cancelled,
}

#[derive(Debug)]
struct CompletionObserver {
    id: u64,
    events: mpsc::Sender<ObservationEvent>,
    phase: Arc<AtomicU8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ObservationPhase {
    Queued = 0,
    Coalesced = 1,
    Running = 2,
    Terminal = 3,
}

impl ObservationPhase {
    fn load(phase: &AtomicU8) -> Self {
        match phase.load(Ordering::Acquire) {
            0 => Self::Queued,
            1 => Self::Coalesced,
            2 => Self::Running,
            _ => Self::Terminal,
        }
    }
}

impl CompletionObserver {
    fn notify(&self, event: ObservationEvent) {
        let phase = match &event {
            ObservationEvent::Coalesced => ObservationPhase::Coalesced,
            ObservationEvent::Running => ObservationPhase::Running,
            ObservationEvent::Completed(_) | ObservationEvent::Cancelled => {
                ObservationPhase::Terminal
            }
        };
        self.phase.store(phase as u8, Ordering::Release);
        let _receiver_was_dropped = self.events.try_send(event);
    }
}

/// The daemon boundary that admitted a reindex request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexSource {
    Startup,
    Watcher,
    Timer,
    Ipc,
}

impl ReindexSource {
    const fn index(self) -> usize {
        match self {
            Self::Startup => 0,
            Self::Watcher => 1,
            Self::Timer => 2,
            Self::Ipc => 3,
        }
    }
}

/// Bounded scheduling policy for [`ReindexCoordinator`].
#[derive(Debug, Clone)]
pub struct ReindexCoordinatorConfig {
    project_paths: ProjectPathSpace,
    debounce: Duration,
    max_debounce: Duration,
    max_dirty_paths: usize,
    max_pending_events: usize,
    max_failure_history: usize,
}

impl ReindexCoordinatorConfig {
    #[must_use]
    pub fn new(project_paths: ProjectPathSpace) -> Self {
        Self {
            project_paths,
            debounce: DEFAULT_DEBOUNCE,
            max_debounce: DEFAULT_MAX_DEBOUNCE,
            max_dirty_paths: DEFAULT_MAX_DIRTY_PATHS,
            max_pending_events: DEFAULT_MAX_PENDING_EVENTS,
            max_failure_history: DEFAULT_MAX_FAILURE_HISTORY,
        }
    }

    #[must_use]
    pub(crate) fn project_id(&self) -> ProjectId {
        self.project_paths.project_id()
    }

    #[must_use]
    pub fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    #[must_use]
    pub fn with_max_debounce(mut self, max_debounce: Duration) -> Self {
        self.max_debounce = max_debounce;
        self
    }

    #[must_use]
    pub fn with_max_dirty_paths(mut self, maximum: usize) -> Self {
        self.max_dirty_paths = maximum;
        self
    }

    #[must_use]
    #[cfg(test)]
    pub fn with_max_pending_events(mut self, maximum: usize) -> Self {
        self.max_pending_events = maximum;
        self
    }

    #[must_use]
    #[cfg(test)]
    pub fn with_max_failure_history(mut self, maximum: usize) -> Self {
        self.max_failure_history = maximum;
        self
    }

    fn validate(self) -> Result<Self, CoordinatorError> {
        if self.debounce.is_zero() {
            return Err(CoordinatorError::InvalidConfiguration(
                "debounce must be greater than zero",
            ));
        }
        if self.max_debounce < self.debounce {
            return Err(CoordinatorError::InvalidConfiguration(
                "max_debounce must be greater than or equal to debounce",
            ));
        }
        if Instant::now().checked_add(self.max_debounce).is_none() {
            return Err(CoordinatorError::InvalidConfiguration(
                "max_debounce exceeds the runtime clock range",
            ));
        }
        for (name, value) in [
            ("max_dirty_paths", self.max_dirty_paths),
            ("max_pending_events", self.max_pending_events),
            ("max_failure_history", self.max_failure_history),
        ] {
            if value == 0 {
                return Err(CoordinatorError::InvalidConfiguration(name));
            }
        }
        Ok(self)
    }
}

/// Stable scope label used by status and failure reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexScopeKind {
    Full,
    Reconcile,
    ChangedPaths,
}

impl ReindexScopeKind {
    const fn from_intent(intent: &FilesystemReindexIntent) -> Self {
        match &intent.scope {
            FilesystemReindexScope::Full => Self::Full,
            FilesystemReindexScope::Reconcile => Self::Reconcile,
            FilesystemReindexScope::ChangedPaths { .. } => Self::ChangedPaths,
        }
    }
}

/// One bounded diagnostic retained after a build failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexFailure {
    pub sequence: u64,
    pub scope: ReindexScopeKind,
    pub message: String,
}

/// Per-boundary admission counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReindexAdmissionCounts {
    pub startup: u64,
    pub watcher: u64,
    pub timer: u64,
    pub ipc: u64,
}

impl ReindexAdmissionCounts {
    fn from_array(counts: [u64; 4]) -> Self {
        Self {
            startup: counts[0],
            watcher: counts[1],
            timer: counts[2],
            ipc: counts[3],
        }
    }
}

/// A bounded point-in-time view of coordinator state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexCoordinatorSnapshot {
    pub running: bool,
    pub in_flight: Option<ReindexScopeKind>,
    pub pending_general: Option<ReindexScopeKind>,
    pub last_completion_failed: bool,
    pub failures: Vec<ReindexFailure>,
    pub full_escalations: u64,
    pub watcher_overflows: u64,
    pub admissions: ReindexAdmissionCounts,
}

impl ReindexCoordinatorSnapshot {
    #[must_use]
    #[cfg(test)]
    pub const fn is_idle(&self) -> bool {
        !self.running && self.in_flight.is_none() && self.pending_general.is_none()
    }
}

/// One terminal build result observed before the coordinator may start another generation.
#[derive(Debug, Clone)]
pub struct ReindexExecution {
    receipt: ReindexReceipt,
    status: StatusResponse,
}

impl ReindexExecution {
    #[must_use]
    pub const fn new(receipt: ReindexReceipt, status: StatusResponse) -> Self {
        Self { receipt, status }
    }

    fn into_parts(self) -> (ReindexReceipt, StatusResponse) {
        (self.receipt, self.status)
    }
}

/// Initial admission and the terminal observation produced by its merged filesystem build.
#[derive(Debug, Clone)]
pub struct ReindexCompletion {
    /// Receipt returned by atomic admission before execution.
    pub admission: ReindexReceipt,
    /// Terminal receipt returned by the concrete executor.
    pub terminal: ReindexReceipt,
    /// Status captured after the terminal receipt and before another build may start.
    pub status: StatusResponse,
}

/// One admitted reindex operation whose terminal result may outlive its requesting connection.
pub struct ReindexObservation {
    admission: ReindexReceipt,
    events: mpsc::Receiver<ObservationEvent>,
    cancellation: ReindexCancellation,
}

impl ReindexObservation {
    #[must_use]
    pub const fn admission(&self) -> &ReindexReceipt {
        &self.admission
    }

    pub(crate) fn cancellation(&self) -> ReindexCancellation {
        self.cancellation.clone()
    }

    pub(crate) async fn next_progress(&mut self) -> ReindexObservationProgress {
        match self.events.recv().await {
            Some(ObservationEvent::Coalesced) => ReindexObservationProgress::Coalesced,
            Some(ObservationEvent::Running) => ReindexObservationProgress::Running,
            Some(ObservationEvent::Cancelled) => ReindexObservationProgress::Cancelled,
            Some(ObservationEvent::Completed(completion)) => {
                ReindexObservationProgress::Terminal(Box::new(match completion.as_ref() {
                    Ok(execution) => {
                        let (terminal, status) = execution.clone().into_parts();
                        Ok(ReindexCompletion {
                            admission: self.admission.clone(),
                            terminal,
                            status,
                        })
                    }
                    Err(failure) => Err(CoordinatorError::ExecutionFailed {
                        admission: Box::new(self.admission.clone()),
                        scope: failure.scope,
                        message: failure.message.clone(),
                    }),
                }))
            }
            None => ReindexObservationProgress::Terminal(Box::new(Err(
                CoordinatorError::CompletionChannelClosed {
                    admission: Box::new(self.admission.clone()),
                },
            ))),
        }
    }
}

pub(crate) enum ReindexObservationProgress {
    Coalesced,
    Running,
    Terminal(Box<Result<ReindexCompletion, CoordinatorError>>),
    Cancelled,
}

#[derive(Clone)]
pub(crate) struct ReindexCancellation {
    inner: Arc<CoordinatorInner>,
    observation_id: u64,
    phase: Arc<AtomicU8>,
}

impl ReindexCancellation {
    pub(crate) async fn cancel(&self) -> ReindexCancellationOutcome {
        let observer = {
            let mut state = self.inner.state.lock().await;
            state.cancel_exclusive(self.observation_id)
        };
        if let Some(observer) = observer {
            observer.notify(ObservationEvent::Cancelled);
            self.inner.changed.notify_waiters();
            self.inner.wake.notify_one();
            return ReindexCancellationOutcome::Cancelled;
        }
        match ObservationPhase::load(&self.phase) {
            ObservationPhase::Coalesced => ReindexCancellationOutcome::Coalesced,
            ObservationPhase::Running => ReindexCancellationOutcome::Running,
            ObservationPhase::Queued | ObservationPhase::Terminal => {
                ReindexCancellationOutcome::Finished
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReindexCancellationOutcome {
    Cancelled,
    Coalesced,
    Running,
    Finished,
}

/// Concrete daemon-owned coordinator.
///
/// The executor is supplied once at construction but erased internally. Callers share this one
/// concrete type rather than defining parallel application-layer scheduling traits.
#[derive(Clone)]
pub struct ReindexCoordinator {
    inner: Arc<CoordinatorInner>,
}

/// Process-lifetime owner for the coordinator executor and its single persistent runner.
#[must_use = "the coordinator runtime must be shut down and joined before daemon leases release"]
pub struct ReindexCoordinatorRuntime {
    coordinator: ReindexCoordinator,
    runner: Option<tokio::task::JoinHandle<()>>,
}

impl fmt::Debug for ReindexCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReindexCoordinator")
            .field("project_paths", &self.inner.config.project_paths)
            .finish_non_exhaustive()
    }
}

impl ReindexCoordinatorRuntime {
    pub fn start<F, Fut>(
        config: ReindexCoordinatorConfig,
        executor: F,
    ) -> Result<Self, CoordinatorError>
    where
        F: Fn(FilesystemReindexIntent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<ReindexExecution>> + Send + 'static,
    {
        let config = config.validate()?;
        let executor: Arc<BuildExecutor> =
            Arc::new(move |intent| Box::pin(executor(intent)) as BuildFuture);
        let inner = Arc::new(CoordinatorInner {
            config,
            state: Mutex::new(CoordinatorState::default()),
            wake: Notify::new(),
            changed: Notify::new(),
            next_observation_id: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
        });
        let runner = tokio::spawn(run_coordinator(Arc::clone(&inner), executor));
        Ok(Self {
            coordinator: ReindexCoordinator { inner },
            runner: Some(runner),
        })
    }

    #[must_use]
    pub fn coordinator(&self) -> ReindexCoordinator {
        self.coordinator.clone()
    }

    /// Closes admission, drains accepted builds, and joins the process-lifetime runner.
    pub async fn shutdown(&mut self) -> Result<(), CoordinatorError> {
        self.coordinator
            .inner
            .shutting_down
            .store(true, Ordering::Release);
        self.coordinator.inner.wake.notify_waiters();
        let Some(runner) = self.runner.as_mut() else {
            return Ok(());
        };
        let result = runner.await;
        self.runner.take();
        result.map_err(|error| CoordinatorError::RunnerTerminated {
            message: truncate_message(error.to_string()),
        })
    }
}

impl Drop for ReindexCoordinatorRuntime {
    fn drop(&mut self) {
        if let Some(runner) = self.runner.take() {
            runner.abort();
        }
    }
}

impl fmt::Debug for ReindexCoordinatorRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReindexCoordinatorRuntime")
            .field("coordinator", &self.coordinator)
            .field("running", &self.runner.is_some())
            .finish()
    }
}

impl ReindexCoordinator {
    #[must_use]
    pub(crate) fn project_path_space(&self) -> &ProjectPathSpace {
        &self.inner.config.project_paths
    }

    /// Atomically admits, rejects, or coalesces one reindex request.
    #[cfg(test)]
    pub async fn admit(
        &self,
        source: ReindexSource,
        intent: FilesystemReindexIntent,
    ) -> Result<ReindexReceipt, CoordinatorError> {
        let prepared = self.prepare_intent(&intent)?;
        self.admit_prepared_unobserved_with(source, prepared, false)
            .await
    }

    /// Atomically admits one request and returns an independently awaitable completion handle.
    #[cfg(test)]
    pub async fn admit_observed(
        &self,
        source: ReindexSource,
        intent: FilesystemReindexIntent,
    ) -> Result<ReindexObservation, CoordinatorError> {
        let prepared = self.prepare_intent(&intent)?;
        self.admit_prepared_observed(source, prepared).await
    }

    pub(crate) fn prepare_intent(
        &self,
        intent: &FilesystemReindexIntent,
    ) -> Result<PreparedReindexIntent, CoordinatorError> {
        let normalized = normalize_general_scope(intent, &self.inner.config)?;
        Ok(PreparedReindexIntent::new(normalized))
    }

    pub(crate) async fn admit_prepared_observed(
        &self,
        source: ReindexSource,
        prepared: PreparedReindexIntent,
    ) -> Result<ReindexObservation, CoordinatorError> {
        self.admit_prepared_observed_with(source, prepared).await
    }

    pub(crate) async fn admit_watcher_overflow_unobserved(
        &self,
    ) -> Result<ReindexReceipt, CoordinatorError> {
        let prepared = PreparedReindexIntent::new(NormalizedGeneralScope {
            scope: GeneralScope::Full,
            escalated: true,
        });
        self.admit_prepared_unobserved_with(ReindexSource::Watcher, prepared, true)
            .await
    }

    async fn admit_prepared_observed_with(
        &self,
        source: ReindexSource,
        prepared: PreparedReindexIntent,
    ) -> Result<ReindexObservation, CoordinatorError> {
        let observation_id = self.next_observation_id();
        let (event_sender, event_receiver) = mpsc::channel(OBSERVATION_EVENT_CAPACITY);
        let phase = Arc::new(AtomicU8::new(ObservationPhase::Queued as u8));
        let observer = CompletionObserver {
            id: observation_id,
            events: event_sender,
            phase: Arc::clone(&phase),
        };
        let (admission, should_start) = {
            let mut state = self.inner.state.lock().await;
            if self.inner.shutting_down.load(Ordering::Acquire) {
                return Err(CoordinatorError::ShuttingDown);
            }
            state.admissions[source.index()] = state.admissions[source.index()].saturating_add(1);
            let escalated = prepared.escalated;
            let disposition = state.admit_general(
                prepared.scope,
                Instant::now(),
                &self.inner.config,
                escalated,
                Some(observer),
            )?;
            if escalated {
                state.full_escalations = state.full_escalations.saturating_add(1);
            }
            let should_start = state.start_runner_if_needed();
            (admission_receipt(disposition), should_start)
        };

        self.signal_runner_after_admission(should_start);
        Ok(ReindexObservation {
            admission,
            events: event_receiver,
            cancellation: ReindexCancellation {
                inner: Arc::clone(&self.inner),
                observation_id,
                phase,
            },
        })
    }

    /// Records a lossy watcher overflow and upgrades pending filesystem work to a full scan.
    #[cfg(test)]
    pub async fn watcher_overflow(&self) -> Result<ReindexReceipt, CoordinatorError> {
        self.admit_watcher_overflow_unobserved().await
    }

    /// Waits until the runner has drained every admitted request.
    #[cfg(test)]
    pub async fn wait_for_idle(&self) {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            let _already_notified = changed.as_mut().enable();
            if self.snapshot().await.is_idle() {
                return;
            }
            changed.await;
        }
    }

    #[must_use]
    pub async fn snapshot(&self) -> ReindexCoordinatorSnapshot {
        let state = self.inner.state.lock().await;
        state.snapshot()
    }

    async fn admit_prepared_unobserved_with(
        &self,
        source: ReindexSource,
        prepared: PreparedReindexIntent,
        watcher_overflow: bool,
    ) -> Result<ReindexReceipt, CoordinatorError> {
        let mut state = self.inner.state.lock().await;
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(CoordinatorError::ShuttingDown);
        }
        state.admissions[source.index()] = state.admissions[source.index()].saturating_add(1);

        if watcher_overflow {
            state.watcher_overflows = state.watcher_overflows.saturating_add(1);
        }
        if prepared.escalated {
            state.full_escalations = state.full_escalations.saturating_add(1);
        }
        let disposition = state.admit_general(
            prepared.scope,
            Instant::now(),
            &self.inner.config,
            prepared.escalated,
            None,
        )?;

        let should_start = state.start_runner_if_needed();
        let receipt = admission_receipt(disposition);
        drop(state);

        self.signal_runner_after_admission(should_start);
        Ok(receipt)
    }

    fn signal_runner_after_admission(&self, should_start: bool) {
        self.inner.changed.notify_waiters();
        self.inner.wake.notify_one();
        let _runner_became_active = should_start;
    }

    fn next_observation_id(&self) -> u64 {
        loop {
            let candidate = self
                .inner
                .next_observation_id
                .fetch_add(1, Ordering::Relaxed);
            if candidate != 0 {
                return candidate;
            }
        }
    }
}

struct CoordinatorInner {
    config: ReindexCoordinatorConfig,
    state: Mutex<CoordinatorState>,
    wake: Notify,
    changed: Notify,
    next_observation_id: AtomicU64,
    shutting_down: AtomicBool,
}

#[derive(Default)]
struct CoordinatorState {
    runner_running: bool,
    in_flight: Option<FilesystemReindexIntent>,
    pending_general: Option<PendingGeneral>,
    last_completion_failed: bool,
    failures: VecDeque<ReindexFailure>,
    failure_sequence: u64,
    full_escalations: u64,
    watcher_overflows: u64,
    admissions: [u64; 4],
    completion_waiter_count: usize,
}

impl CoordinatorState {
    fn admit_general(
        &mut self,
        incoming: GeneralScope,
        now: Instant,
        config: &ReindexCoordinatorConfig,
        force_immediate: bool,
        completion_waiter: Option<CompletionObserver>,
    ) -> Result<ReindexDisposition, CoordinatorError> {
        if completion_waiter.is_some() && self.completion_waiter_count >= config.max_pending_events
        {
            return Err(CoordinatorError::CompletionWaiterLimit {
                maximum: config.max_pending_events,
            });
        }

        let Some(pending) = self.pending_general.as_mut() else {
            let mut pending = PendingGeneral::new(incoming, now, force_immediate);
            if let Some(waiter) = completion_waiter {
                pending.try_push_waiter(waiter)?;
                self.completion_waiter_count += 1;
            }
            self.pending_general = Some(pending);
            return Ok(ReindexDisposition::Queued);
        };

        let added_observer = completion_waiter.is_some();
        if let Some(observer) = completion_waiter {
            pending.try_push_waiter(observer)?;
            self.completion_waiter_count += 1;
        }
        pending.last_event = now;
        pending.event_count = pending.event_count.saturating_add(1);
        pending.shared = true;
        if !pending.coalesced_notified {
            pending.notify_observers(ObservationEvent::Coalesced);
            pending.coalesced_notified = true;
        } else if added_observer {
            pending
                .waiters
                .last()
                .expect("the admitted observer was appended")
                .notify(ObservationEvent::Coalesced);
        }
        pending.force_immediate |= force_immediate;
        if pending.scope.merge(incoming, config.max_dirty_paths) {
            self.full_escalations = self.full_escalations.saturating_add(1);
            pending.force_immediate = true;
        }
        if pending.event_count > config.max_pending_events {
            if !matches!(pending.scope, GeneralScope::Full) {
                self.full_escalations = self.full_escalations.saturating_add(1);
            }
            pending.scope = GeneralScope::Full;
            pending.event_count = config.max_pending_events;
            pending.force_immediate = true;
        }
        Ok(ReindexDisposition::Coalesced)
    }

    fn start_runner_if_needed(&mut self) -> bool {
        let should_start = !self.runner_running && self.pending_general.is_some();
        if should_start {
            self.runner_running = true;
        }
        should_start
    }

    fn take_ready(&mut self, now: Instant, config: &ReindexCoordinatorConfig) -> RunnerAction {
        let general_is_ready = self
            .pending_general
            .as_ref()
            .is_some_and(|pending| pending.ready_at(config) <= now);
        if general_is_ready && let Some(pending) = self.pending_general.take() {
            let PendingGeneral { scope, waiters, .. } = pending;
            for observer in &waiters {
                observer.notify(ObservationEvent::Running);
            }
            let intent = scope.into_intent();
            self.in_flight = Some(intent.clone());
            return RunnerAction::Execute {
                intent: Box::new(intent),
                waiters,
            };
        }

        if let Some(pending) = self.pending_general.as_ref() {
            return RunnerAction::WaitUntil(pending.ready_at(config));
        }

        self.runner_running = false;
        RunnerAction::Stop
    }

    fn cancel_exclusive(&mut self, observation_id: u64) -> Option<CompletionObserver> {
        let pending = self.pending_general.as_ref()?;
        if pending.shared || pending.waiters.len() != 1 || pending.waiters[0].id != observation_id {
            return None;
        }
        let mut pending = self.pending_general.take()?;
        self.completion_waiter_count = self.completion_waiter_count.saturating_sub(1);
        pending.waiters.pop()
    }

    fn finish(
        &mut self,
        intent: &FilesystemReindexIntent,
        outcome: &CompletionOutcome,
        completed_waiters: usize,
        config: &ReindexCoordinatorConfig,
    ) {
        self.in_flight = None;
        self.last_completion_failed = outcome.is_err();
        self.completion_waiter_count = self
            .completion_waiter_count
            .saturating_sub(completed_waiters);
        if let Err(failure) = outcome {
            self.record_failure(intent, failure.message.clone(), config.max_failure_history);
        }
    }

    fn record_failure(
        &mut self,
        intent: &FilesystemReindexIntent,
        message: String,
        maximum: usize,
    ) {
        self.failure_sequence = self.failure_sequence.saturating_add(1);
        self.failures.push_back(ReindexFailure {
            sequence: self.failure_sequence,
            scope: ReindexScopeKind::from_intent(intent),
            message: truncate_message(message),
        });
        while self.failures.len() > maximum {
            self.failures.pop_front();
        }
    }

    fn snapshot(&self) -> ReindexCoordinatorSnapshot {
        ReindexCoordinatorSnapshot {
            running: self.runner_running,
            in_flight: self.in_flight.as_ref().map(ReindexScopeKind::from_intent),
            pending_general: self
                .pending_general
                .as_ref()
                .map(|pending| pending.scope.kind()),
            last_completion_failed: self.last_completion_failed,
            failures: self.failures.iter().cloned().collect(),
            full_escalations: self.full_escalations,
            watcher_overflows: self.watcher_overflows,
            admissions: ReindexAdmissionCounts::from_array(self.admissions),
        }
    }
}

enum RunnerAction {
    Execute {
        intent: Box<FilesystemReindexIntent>,
        waiters: Vec<CompletionObserver>,
    },
    WaitUntil(Instant),
    Stop,
}

async fn run_coordinator(inner: Arc<CoordinatorInner>, executor: Arc<BuildExecutor>) {
    loop {
        let notified = inner.wake.notified();
        let action = {
            let mut state = inner.state.lock().await;
            state.take_ready(Instant::now(), &inner.config)
        };

        match action {
            RunnerAction::Execute { intent, waiters } => {
                inner.changed.notify_waiters();
                let outcome = Arc::new(execute(&executor, &intent).await);
                let mut state = inner.state.lock().await;
                state.finish(&intent, outcome.as_ref(), waiters.len(), &inner.config);
                drop(state);
                for observer in waiters {
                    observer.notify(ObservationEvent::Completed(Arc::clone(&outcome)));
                }
                inner.changed.notify_waiters();
                inner.wake.notify_one();
            }
            RunnerAction::WaitUntil(deadline) => {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => {}
                    () = notified => {}
                }
            }
            RunnerAction::Stop => {
                inner.changed.notify_waiters();
                if inner.shutting_down.load(Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ExecutionFailure {
    scope: ReindexScopeKind,
    message: String,
}

impl ExecutionFailure {
    fn new(intent: &FilesystemReindexIntent, message: String) -> Self {
        Self {
            scope: ReindexScopeKind::from_intent(intent),
            message: truncate_message(message),
        }
    }
}

async fn execute(
    executor: &Arc<BuildExecutor>,
    intent: &FilesystemReindexIntent,
) -> CompletionOutcome {
    let execution_intent = intent.clone();
    let outcome = match catch_unwind(AssertUnwindSafe(|| executor(execution_intent))) {
        Ok(future) => match AssertUnwindSafe(future).catch_unwind().await {
            Ok(Ok(execution)) => validate_execution(execution),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("reindex executor future panicked".to_owned()),
        },
        Err(_) => Err("reindex executor panicked before returning its future".to_owned()),
    };
    outcome.map_err(|message| ExecutionFailure::new(intent, message))
}

fn validate_execution(execution: ReindexExecution) -> Result<ReindexExecution, String> {
    let receipt = &execution.receipt;
    receipt
        .validate()
        .map_err(|error| format!("executor returned an invalid receipt: {error}"))?;
    if receipt.transaction.is_some() {
        return Err("executor returned a receipt for a different transaction".to_owned());
    }
    if receipt.target_revision.is_some() {
        return Err("executor returned a receipt for a different target revision".to_owned());
    }
    if !matches!(
        receipt.disposition,
        ReindexDisposition::Applied | ReindexDisposition::AlreadyApplied
    ) {
        return Err(format!(
            "executor returned non-terminal disposition {:?}",
            receipt.disposition
        ));
    }
    execution
        .status
        .validate()
        .map_err(|error| format!("executor returned an invalid status: {error}"))?;
    if execution.status.indexing || execution.status.generation.building_revision.is_some() {
        return Err("executor returned a non-terminal status snapshot".to_owned());
    }
    if execution.status.generation.active != receipt.generation {
        return Err("executor receipt and status identify different generations".to_owned());
    }
    Ok(execution)
}

#[derive(Debug)]
struct NormalizedGeneralScope {
    scope: GeneralScope,
    escalated: bool,
}

pub(crate) struct PreparedReindexIntent {
    scope: GeneralScope,
    escalated: bool,
    fingerprint: [u8; 32],
}

impl PreparedReindexIntent {
    fn new(normalized: NormalizedGeneralScope) -> Self {
        let fingerprint = reindex_intent_fingerprint(&normalized.scope);
        Self {
            scope: normalized.scope,
            escalated: normalized.escalated,
            fingerprint,
        }
    }

    pub(crate) const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

fn normalize_general_scope(
    intent: &FilesystemReindexIntent,
    config: &ReindexCoordinatorConfig,
) -> Result<NormalizedGeneralScope, CoordinatorError> {
    match &intent.scope {
        FilesystemReindexScope::Full => Ok(NormalizedGeneralScope {
            scope: GeneralScope::Full,
            escalated: false,
        }),
        FilesystemReindexScope::Reconcile => Ok(NormalizedGeneralScope {
            scope: GeneralScope::Reconcile,
            escalated: false,
        }),
        FilesystemReindexScope::ChangedPaths { paths } => {
            if paths.project_id() != config.project_paths.project_id() {
                return Err(CoordinatorError::ChangedPathProjectMismatch {
                    expected: config.project_paths.project_id(),
                    actual: paths.project_id(),
                });
            }
            if paths.len() > config.max_dirty_paths {
                return Ok(NormalizedGeneralScope {
                    scope: GeneralScope::Full,
                    escalated: true,
                });
            }
            if paths.is_empty() {
                return Ok(NormalizedGeneralScope {
                    scope: GeneralScope::Reconcile,
                    escalated: false,
                });
            }
            Ok(NormalizedGeneralScope {
                scope: GeneralScope::ChangedPaths(paths.clone()),
                escalated: false,
            })
        }
    }
}

#[derive(Debug)]
enum GeneralScope {
    Full,
    Reconcile,
    ChangedPaths(ProjectPathSet),
}

fn reindex_intent_fingerprint(scope: &GeneralScope) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(REINDEX_INTENT_FINGERPRINT_DOMAIN);
    match scope {
        GeneralScope::Full => hasher.update([0]),
        GeneralScope::Reconcile => hasher.update([1]),
        GeneralScope::ChangedPaths(paths) => {
            hasher.update([2]);
            hasher.update((paths.len() as u64).to_le_bytes());
            for path in paths.iter() {
                hasher.update(path.identity().as_bytes());
            }
        }
    }
    hasher.finalize().into()
}

impl GeneralScope {
    fn merge(&mut self, incoming: Self, max_dirty_paths: usize) -> bool {
        if matches!(self, Self::Full) {
            return false;
        }
        match incoming {
            Self::Full => {
                *self = Self::Full;
                false
            }
            Self::Reconcile => {
                if !matches!(self, Self::Reconcile) {
                    *self = Self::Reconcile;
                }
                false
            }
            Self::ChangedPaths(incoming) => match self {
                Self::Full | Self::Reconcile => false,
                Self::ChangedPaths(current) => {
                    current
                        .extend(incoming)
                        .expect("coordinator admits paths from one project space");
                    if current.len() > max_dirty_paths {
                        *self = Self::Full;
                        true
                    } else {
                        false
                    }
                }
            },
        }
    }

    const fn kind(&self) -> ReindexScopeKind {
        match self {
            Self::Full => ReindexScopeKind::Full,
            Self::Reconcile => ReindexScopeKind::Reconcile,
            Self::ChangedPaths(_) => ReindexScopeKind::ChangedPaths,
        }
    }

    fn into_intent(self) -> FilesystemReindexIntent {
        match self {
            Self::Full => FilesystemReindexIntent::full(),
            Self::Reconcile => FilesystemReindexIntent::reconcile(),
            Self::ChangedPaths(paths) => FilesystemReindexIntent::changed_paths(paths),
        }
    }
}

#[derive(Debug)]
struct PendingGeneral {
    scope: GeneralScope,
    first_admitted: Instant,
    last_event: Instant,
    event_count: usize,
    shared: bool,
    coalesced_notified: bool,
    force_immediate: bool,
    waiters: Vec<CompletionObserver>,
}

impl PendingGeneral {
    fn new(scope: GeneralScope, now: Instant, force_immediate: bool) -> Self {
        Self {
            scope,
            first_admitted: now,
            last_event: now,
            event_count: 1,
            shared: false,
            coalesced_notified: false,
            force_immediate,
            waiters: Vec::new(),
        }
    }

    fn try_push_waiter(&mut self, waiter: CompletionObserver) -> Result<(), CoordinatorError> {
        self.waiters
            .try_reserve(1)
            .map_err(|_| CoordinatorError::CompletionWaiterAllocationFailed)?;
        self.waiters.push(waiter);
        Ok(())
    }

    fn notify_observers(&self, event: ObservationEvent) {
        for observer in &self.waiters {
            observer.notify(event.clone());
        }
    }

    fn ready_at(&self, config: &ReindexCoordinatorConfig) -> Instant {
        if self.force_immediate {
            return self.first_admitted;
        }
        let quiet_deadline = self
            .last_event
            .checked_add(config.debounce)
            .unwrap_or(self.last_event);
        let absolute_deadline = self
            .first_admitted
            .checked_add(config.max_debounce)
            .unwrap_or(self.first_admitted);
        quiet_deadline.min(absolute_deadline)
    }
}

fn admission_receipt(disposition: ReindexDisposition) -> ReindexReceipt {
    ReindexReceipt {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        disposition,
        transaction: None,
        target_revision: None,
        generation: None,
        evidence: Default::default(),
    }
}

fn truncate_message(mut message: String) -> String {
    if message.len() <= MAX_FAILURE_MESSAGE_BYTES {
        return message;
    }
    let mut boundary = MAX_FAILURE_MESSAGE_BYTES;
    while !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message.truncate(boundary);
    message
}

/// Admission and execution failures owned by the daemon coordinator.
#[derive(Debug)]
#[non_exhaustive]
pub enum CoordinatorError {
    InvalidConfiguration(&'static str),
    ShuttingDown,
    RunnerTerminated {
        message: String,
    },
    CompletionWaiterLimit {
        maximum: usize,
    },
    CompletionWaiterAllocationFailed,
    ExecutionFailed {
        admission: Box<ReindexReceipt>,
        scope: ReindexScopeKind,
        message: String,
    },
    CompletionChannelClosed {
        admission: Box<ReindexReceipt>,
    },
    ChangedPathProjectMismatch {
        expected: ProjectId,
        actual: ProjectId,
    },
}

impl fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(
                    formatter,
                    "invalid reindex coordinator configuration: {message}"
                )
            }
            Self::ShuttingDown => formatter.write_str("reindex coordinator is shutting down"),
            Self::RunnerTerminated { message } => {
                write!(
                    formatter,
                    "reindex coordinator runner terminated: {message}"
                )
            }
            Self::CompletionWaiterLimit { maximum } => write!(
                formatter,
                "reindex completion waiter limit reached; maximum pending waiters is {maximum}"
            ),
            Self::CompletionWaiterAllocationFailed => {
                formatter.write_str("could not allocate a reindex completion waiter")
            }
            Self::ExecutionFailed { scope, message, .. } => {
                write!(formatter, "reindex {scope:?} execution failed: {message}")
            }
            Self::CompletionChannelClosed { .. } => {
                formatter.write_str("reindex completion channel closed before reporting a result")
            }
            Self::ChangedPathProjectMismatch { expected, actual } => write!(
                formatter,
                "changed paths belong to project {actual}, but this coordinator owns {expected}"
            ),
        }
    }
}

impl Error for CoordinatorError {}

#[cfg(test)]
mod tests;
