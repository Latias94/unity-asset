//! Lifecycle-owned reindex operation admission, observation, and retention.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, RwLock, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use unity_asset_search_index::FilesystemReindexIntent;
use unity_asset_search_protocol::{
    BackgroundReindexOperation, BackgroundReindexOrigin, DaemonInstanceId, OperationId,
    ReindexOperationState, ReindexReceipt, StatusResponse,
};

use crate::coordinator::{
    CoordinatorError, PreparedReindexIntent, ReindexCancellation, ReindexCancellationOutcome,
    ReindexCoordinator, ReindexCoordinatorSnapshot, ReindexObservation, ReindexObservationProgress,
    ReindexScopeKind, ReindexSource,
};
use crate::lifecycle::AdmissionGate;

const MAX_ACTIVE_OPERATIONS: usize = 256;
const MAX_ACTIVE_CLIENT_OPERATIONS: usize = 240;
const MAX_RETAINED_TERMINAL_OPERATIONS: usize = 256;
const MAX_RETAINED_EXPIRED_OPERATIONS: usize = 256;
const TERMINAL_OPERATION_RETENTION: Duration = Duration::from_secs(10 * 60);
const EXPIRED_OPERATION_RETENTION: Duration = Duration::from_secs(10 * 60);
const OPERATION_EPOCH_BYTES: usize = 8;
const INITIAL_SEMANTIC_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const MAXIMUM_SEMANTIC_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// The daemon authority that admitted an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperationOrigin {
    Startup,
    Watcher,
    WatcherOverflow,
    Timer,
    SemanticUpgrade,
    Ipc,
}

impl OperationOrigin {
    const fn coordinator_source(self) -> ReindexSource {
        match self {
            Self::Startup => ReindexSource::Startup,
            Self::Watcher | Self::WatcherOverflow => ReindexSource::Watcher,
            Self::Timer => ReindexSource::Timer,
            Self::SemanticUpgrade => ReindexSource::SemanticUpgrade,
            Self::Ipc => ReindexSource::Ipc,
        }
    }

    const fn active_limit(self, retention: OperationRetentionPolicy) -> usize {
        match self {
            Self::Ipc => retention.maximum_client_active,
            Self::Startup
            | Self::Watcher
            | Self::WatcherOverflow
            | Self::Timer
            | Self::SemanticUpgrade => retention.maximum_active,
        }
    }

    const fn background(self) -> Option<BackgroundReindexOrigin> {
        match self {
            Self::Startup => Some(BackgroundReindexOrigin::Startup),
            Self::Watcher => Some(BackgroundReindexOrigin::Watcher),
            Self::WatcherOverflow => Some(BackgroundReindexOrigin::WatcherOverflow),
            Self::Timer => Some(BackgroundReindexOrigin::Timer),
            Self::SemanticUpgrade => Some(BackgroundReindexOrigin::SemanticUpgrade),
            Self::Ipc => None,
        }
    }

    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Watcher => "watcher",
            Self::WatcherOverflow => "watcher_overflow",
            Self::Timer => "timer",
            Self::SemanticUpgrade => "semantic_upgrade",
            Self::Ipc => "ipc",
        }
    }
}

/// Typed terminal evidence retained independently of the transport DTO.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OperationFailure {
    #[error("{scope:?} reindex execution failed: {message}")]
    Execution {
        scope: ReindexScopeKind,
        message: String,
    },
    #[error("reindex completion channel closed unexpectedly")]
    CompletionChannelClosed,
}

/// One queryable operation state. Every retained daemon operation has a typed origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationSnapshot {
    pub origin: Option<OperationOrigin>,
    pub operation_id: OperationId,
    pub state: ReindexOperationState,
    pub admission: Option<ReindexReceipt>,
    pub completion: Option<ReindexReceipt>,
    pub status: Option<StatusResponse>,
    pub failure: Option<OperationFailure>,
}

impl OperationSnapshot {
    fn active(
        origin: OperationOrigin,
        operation_id: OperationId,
        state: ReindexOperationState,
        admission: ReindexReceipt,
    ) -> Self {
        Self {
            origin: Some(origin),
            operation_id,
            state,
            admission: Some(admission),
            completion: None,
            status: None,
            failure: None,
        }
    }

    fn terminal_marker(
        origin: Option<OperationOrigin>,
        operation_id: OperationId,
        state: ReindexOperationState,
    ) -> Self {
        debug_assert!(matches!(
            state,
            ReindexOperationState::Expired | ReindexOperationState::Lost
        ));
        Self {
            origin,
            operation_id,
            state,
            admission: None,
            completion: None,
            status: None,
            failure: None,
        }
    }

    #[must_use]
    pub fn semantics_are_current(&self) -> bool {
        match &self.status {
            Some(status) => match &status.generation.active {
                Some(generation) => {
                    generation.semantics_current && generation.configuration_current
                }
                None => false,
            },
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationCancellation {
    pub operation_id: OperationId,
    pub state: ReindexOperationState,
    pub cancelled: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    #[error("operation service is draining")]
    Draining,
    #[error("reindex operation active limit reached; maximum is {maximum}")]
    Saturated { maximum: usize },
    #[error("idempotency key is already bound to operation {operation_id} with another intent")]
    IdempotencyConflict { operation_id: OperationId },
    #[error("operation completion task terminated unexpectedly: {message}")]
    CompletionTaskTerminated { message: String },
    #[error("reindex operation was not found")]
    NotFound,
    #[error("{origin:?} reindex operation is daemon-owned and cannot be cancelled by a client")]
    ControlForbidden { origin: OperationOrigin },
    #[error("operation registry invariant failed: {message}")]
    RegistryInvariant { message: &'static str },
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
}

impl OperationError {
    #[must_use]
    pub fn is_retryable_admission(&self) -> bool {
        matches!(
            self,
            Self::Saturated { .. }
                | Self::Coordinator(CoordinatorError::CompletionWaiterLimit { .. })
                | Self::Coordinator(CoordinatorError::CompletionWaiterAllocationFailed)
        )
    }
}

#[derive(Clone)]
pub struct OperationService {
    admission_gate: Arc<Mutex<()>>,
    state: Arc<Mutex<RegistryState>>,
    operation_epoch: [u8; OPERATION_EPOCH_BYTES],
    retention: OperationRetentionPolicy,
    tasks: Arc<Mutex<OperationTaskState>>,
    coordinator: ReindexCoordinator,
    lifecycle_admission: AdmissionGate,
}

struct OperationTaskState {
    accepting: bool,
    tasks: JoinSet<()>,
}

#[must_use = "operation completion tasks must be joined before daemon leases release"]
pub struct OperationServiceOwner {
    service: OperationService,
    draining: Option<JoinSet<()>>,
}

#[derive(Default)]
struct RegistryState {
    entries: BTreeMap<OperationId, OperationRecord>,
    terminal_order: VecDeque<(Instant, OperationId)>,
    expired_order: VecDeque<(Instant, OperationId)>,
    idempotency: BTreeMap<String, IdempotencyBinding>,
    background_by_origin: BTreeMap<BackgroundReindexOrigin, VecDeque<OperationId>>,
    active_count: usize,
    completion_task_failure: Option<String>,
}

#[derive(Clone, Copy)]
struct IdempotencyBinding {
    operation_id: OperationId,
    intent_fingerprint: [u8; 32],
}

#[derive(Clone, Copy)]
pub(crate) struct OperationRetentionPolicy {
    pub(crate) maximum_active: usize,
    pub(crate) maximum_client_active: usize,
    pub(crate) maximum_terminal: usize,
    pub(crate) maximum_expired: usize,
    pub(crate) terminal_retention: Duration,
    pub(crate) expired_retention: Duration,
}

impl Default for OperationRetentionPolicy {
    fn default() -> Self {
        Self {
            maximum_active: MAX_ACTIVE_OPERATIONS,
            maximum_client_active: MAX_ACTIVE_CLIENT_OPERATIONS,
            maximum_terminal: MAX_RETAINED_TERMINAL_OPERATIONS,
            maximum_expired: MAX_RETAINED_EXPIRED_OPERATIONS,
            terminal_retention: TERMINAL_OPERATION_RETENTION,
            expired_retention: EXPIRED_OPERATION_RETENTION,
        }
    }
}

enum OperationRecord {
    Active(Arc<OperationEntry>),
    Terminal(Arc<OperationEntry>),
    Expired(Box<OperationSnapshot>),
}

struct OperationEntry {
    origin: OperationOrigin,
    status: RwLock<OperationSnapshot>,
    changed: Notify,
    cancellation: ReindexCancellation,
}

impl OperationService {
    pub fn new(
        daemon_instance_id: DaemonInstanceId,
        coordinator: ReindexCoordinator,
        lifecycle_admission: AdmissionGate,
    ) -> Self {
        Self::with_retention(
            daemon_instance_id,
            coordinator,
            lifecycle_admission,
            OperationRetentionPolicy::default(),
        )
    }

    pub(crate) fn with_retention(
        daemon_instance_id: DaemonInstanceId,
        coordinator: ReindexCoordinator,
        lifecycle_admission: AdmissionGate,
        retention: OperationRetentionPolicy,
    ) -> Self {
        let mut operation_epoch = [0_u8; OPERATION_EPOCH_BYTES];
        operation_epoch.copy_from_slice(&daemon_instance_id.as_bytes()[..OPERATION_EPOCH_BYTES]);
        Self {
            admission_gate: Arc::new(Mutex::new(())),
            state: Arc::new(Mutex::new(RegistryState::default())),
            operation_epoch,
            retention,
            tasks: Arc::new(Mutex::new(OperationTaskState {
                accepting: true,
                tasks: JoinSet::new(),
            })),
            coordinator,
            lifecycle_admission,
        }
    }

    #[must_use]
    pub fn project_path_space(&self) -> &unity_asset_search_index::ProjectPathSpace {
        self.coordinator.project_path_space()
    }

    pub async fn admit(
        &self,
        origin: OperationOrigin,
        intent: FilesystemReindexIntent,
        idempotency_key: Option<String>,
    ) -> Result<OperationSnapshot, OperationError> {
        debug_assert_ne!(origin, OperationOrigin::WatcherOverflow);
        let prepared = self.coordinator.prepare_intent(&intent)?;
        self.admit_prepared(origin, prepared, idempotency_key, false)
            .await
    }

    pub(crate) async fn admit_timer_and_wait(&self) -> Result<OperationSnapshot, OperationError> {
        let admitted = self
            .admit(
                OperationOrigin::Timer,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await?;
        self.wait_until_terminal(admitted.operation_id).await
    }

    pub async fn admit_watcher_overflow(&self) -> Result<OperationSnapshot, OperationError> {
        let prepared = self
            .coordinator
            .prepare_intent(&FilesystemReindexIntent::full())?;
        self.admit_prepared(OperationOrigin::WatcherOverflow, prepared, None, true)
            .await
    }

    async fn admit_prepared(
        &self,
        origin: OperationOrigin,
        prepared: PreparedReindexIntent,
        idempotency_key: Option<String>,
        watcher_overflow: bool,
    ) -> Result<OperationSnapshot, OperationError> {
        let _lifecycle_admission = self
            .lifecycle_admission
            .admit()
            .await
            .ok_or(OperationError::Draining)?;
        let _admission = self.admission_gate.lock().await;
        self.ensure_completion_tasks_healthy().await?;
        let mut tasks = self.tasks.lock().await;
        if !tasks.accepting {
            return Err(OperationError::Draining);
        }

        let intent_fingerprint = prepared.fingerprint();
        let mut registry = self.state.lock().await;
        registry.prune(Instant::now(), self.retention);
        let existing = idempotency_key
            .as_ref()
            .and_then(|key| registry.idempotency.get(key).copied());
        let maximum = origin.active_limit(self.retention);
        if existing.is_none() && registry.active_count >= maximum {
            return Err(OperationError::Saturated { maximum });
        }
        if let Some(existing) = existing {
            if existing.intent_fingerprint != intent_fingerprint {
                return Err(OperationError::IdempotencyConflict {
                    operation_id: existing.operation_id,
                });
            }
            drop(registry);
            drop(tasks);
            return self.status(existing.operation_id).await;
        }

        let operation_id = self.unique_operation_id(&registry);
        // The coordinator mutates no state before its single mutex wait completes. Holding the
        // registry and task-owner guards across that wait makes the following handoff atomic from
        // the caller's perspective: once the coordinator accepts an observer, this future reaches
        // registry insertion and owned task installation without another cancellation point.
        let observation = if watcher_overflow {
            self.coordinator.admit_watcher_overflow_observed().await?
        } else {
            self.coordinator
                .admit_prepared_observed(origin.coordinator_source(), prepared)
                .await?
        };
        let cancellation = observation.cancellation();
        let state = match observation.admission().disposition {
            unity_asset_search_protocol::ReindexDisposition::Coalesced => {
                ReindexOperationState::Coalesced
            }
            unity_asset_search_protocol::ReindexDisposition::Queued => {
                ReindexOperationState::Queued
            }
            unity_asset_search_protocol::ReindexDisposition::Applied
            | unity_asset_search_protocol::ReindexDisposition::AlreadyApplied => {
                ReindexOperationState::Running
            }
        };
        let status =
            OperationSnapshot::active(origin, operation_id, state, observation.admission().clone());
        let entry = Arc::new(OperationEntry {
            origin,
            status: RwLock::new(status.clone()),
            changed: Notify::new(),
            cancellation,
        });
        registry
            .entries
            .insert(operation_id, OperationRecord::Active(Arc::clone(&entry)));
        registry.active_count = registry.active_count.saturating_add(1);
        if let Some(background_origin) = origin.background() {
            registry
                .background_by_origin
                .entry(background_origin)
                .or_default()
                .push_back(operation_id);
        }
        if let Some(key) = idempotency_key {
            registry.idempotency.insert(
                key,
                IdempotencyBinding {
                    operation_id,
                    intent_fingerprint,
                },
            );
        }
        tasks.tasks.spawn(complete_operation(
            entry,
            observation,
            operation_id,
            Arc::clone(&self.state),
            self.retention,
        ));
        Ok(status)
    }

    pub async fn status(
        &self,
        operation_id: OperationId,
    ) -> Result<OperationSnapshot, OperationError> {
        self.ensure_completion_tasks_healthy().await?;
        match self.lookup(operation_id).await {
            Some(OperationLookup::Entry(entry)) => Ok(entry.status.read().await.clone()),
            Some(OperationLookup::Snapshot(status)) => Ok(*status),
            None if self.belongs_to_current_epoch(operation_id) => Err(OperationError::NotFound),
            None => Ok(OperationSnapshot::terminal_marker(
                None,
                operation_id,
                ReindexOperationState::Lost,
            )),
        }
    }

    pub async fn background_operations(
        &self,
    ) -> Result<Vec<BackgroundReindexOperation>, OperationError> {
        self.ensure_completion_tasks_healthy().await?;
        let captured = {
            let mut registry = self.state.lock().await;
            registry.prune(Instant::now(), self.retention);
            let mut captured = Vec::with_capacity(registry.background_by_origin.len());
            for (&origin, operation_ids) in &registry.background_by_origin {
                let Some(operation_id) = operation_ids.back().copied() else {
                    continue;
                };
                let Some(record) = registry.entries.get(&operation_id) else {
                    return Err(OperationError::RegistryInvariant {
                        message: "background operation is missing from the retained registry",
                    });
                };
                captured.push((origin, operation_id, record.lookup()));
            }
            captured
        };

        let mut operations = Vec::with_capacity(captured.len());
        for (origin, operation_id, lookup) in captured {
            let state = match lookup {
                OperationLookup::Entry(entry) => entry.status.read().await.state,
                OperationLookup::Snapshot(snapshot) => snapshot.state,
            };
            operations.push(BackgroundReindexOperation {
                origin,
                operation_id,
                state,
            });
        }
        Ok(operations)
    }

    pub async fn wait(
        &self,
        operation_id: OperationId,
        timeout: Duration,
    ) -> Result<OperationSnapshot, OperationError> {
        self.ensure_completion_tasks_healthy().await?;
        let entry = match self.lookup(operation_id).await {
            Some(OperationLookup::Entry(entry)) => entry,
            Some(OperationLookup::Snapshot(status)) => return Ok(*status),
            None if self.belongs_to_current_epoch(operation_id) => {
                return Err(OperationError::NotFound);
            }
            None => {
                return Ok(OperationSnapshot::terminal_marker(
                    None,
                    operation_id,
                    ReindexOperationState::Lost,
                ));
            }
        };
        match tokio::time::timeout(timeout, wait_for_terminal(&entry)).await {
            Ok(status) => Ok(status),
            Err(_) => {
                self.ensure_completion_tasks_healthy().await?;
                Ok(entry.status.read().await.clone())
            }
        }
    }

    async fn wait_until_terminal(
        &self,
        operation_id: OperationId,
    ) -> Result<OperationSnapshot, OperationError> {
        loop {
            self.ensure_completion_tasks_healthy().await?;
            match self.lookup(operation_id).await {
                Some(OperationLookup::Entry(entry)) => {
                    if let Ok(status) =
                        tokio::time::timeout(Duration::from_secs(1), wait_for_terminal(&entry))
                            .await
                    {
                        return Ok(status);
                    }
                }
                Some(OperationLookup::Snapshot(status)) => return Ok(*status),
                None if self.belongs_to_current_epoch(operation_id) => {
                    return Err(OperationError::NotFound);
                }
                None => {
                    return Ok(OperationSnapshot::terminal_marker(
                        None,
                        operation_id,
                        ReindexOperationState::Lost,
                    ));
                }
            }
        }
    }

    pub async fn cancel(
        &self,
        operation_id: OperationId,
    ) -> Result<OperationCancellation, OperationError> {
        self.ensure_completion_tasks_healthy().await?;
        let entry = match self.lookup(operation_id).await {
            Some(OperationLookup::Entry(entry)) => {
                if entry.origin != OperationOrigin::Ipc {
                    return Err(OperationError::ControlForbidden {
                        origin: entry.origin,
                    });
                }
                entry
            }
            Some(OperationLookup::Snapshot(status)) => {
                if let Some(origin) = status.origin
                    && origin != OperationOrigin::Ipc
                {
                    return Err(OperationError::ControlForbidden { origin });
                }
                return Ok(OperationCancellation {
                    operation_id,
                    state: status.state,
                    cancelled: false,
                });
            }
            None if self.belongs_to_current_epoch(operation_id) => {
                return Err(OperationError::NotFound);
            }
            None => {
                return Ok(OperationCancellation {
                    operation_id,
                    state: ReindexOperationState::Lost,
                    cancelled: false,
                });
            }
        };
        let outcome = entry.cancellation.cancel().await;
        let mut status = entry.status.write().await;
        match outcome {
            ReindexCancellationOutcome::Cancelled if !status.state.is_terminal() => {
                status.state = ReindexOperationState::Cancelled;
                status.completion = None;
                status.status = None;
                status.failure = None;
            }
            ReindexCancellationOutcome::Coalesced => {
                advance_active_state(&mut status, ReindexOperationState::Coalesced);
            }
            ReindexCancellationOutcome::Running => {
                advance_active_state(&mut status, ReindexOperationState::Running);
            }
            ReindexCancellationOutcome::Cancelled | ReindexCancellationOutcome::Finished => {}
        }
        let await_terminal =
            outcome == ReindexCancellationOutcome::Finished && !status.state.is_terminal();
        drop(status);
        if outcome == ReindexCancellationOutcome::Cancelled {
            self.state
                .lock()
                .await
                .mark_terminal(operation_id, Instant::now(), self.retention);
        }
        if outcome != ReindexCancellationOutcome::Finished {
            entry.changed.notify_waiters();
        }
        let status = if await_terminal {
            self.wait_until_terminal(operation_id).await?
        } else {
            entry.status.read().await.clone()
        };
        Ok(OperationCancellation {
            operation_id,
            state: status.state,
            cancelled: status.state == ReindexOperationState::Cancelled,
        })
    }

    pub async fn coordinator_snapshot(&self) -> ReindexCoordinatorSnapshot {
        self.coordinator.snapshot().await
    }

    async fn ensure_completion_tasks_healthy(&self) -> Result<(), OperationError> {
        let failure = {
            let mut tasks = self.tasks.lock().await;
            let mut first_failure = None;
            while let Some(result) = tasks.tasks.try_join_next() {
                if let Err(error) = result
                    && first_failure.is_none()
                {
                    first_failure = Some(crate::truncate_utf8(error.to_string(), 4 * 1024));
                }
            }
            first_failure
        };
        if let Some(message) = failure {
            let mut state = self.state.lock().await;
            if state.completion_task_failure.is_none() {
                state.completion_task_failure = Some(message);
            }
        }
        match self.state.lock().await.completion_task_failure.clone() {
            Some(message) => Err(OperationError::CompletionTaskTerminated { message }),
            None => Ok(()),
        }
    }

    fn unique_operation_id(&self, registry: &RegistryState) -> OperationId {
        loop {
            let mut bytes = rand::random::<[u8; 16]>();
            bytes[..OPERATION_EPOCH_BYTES].copy_from_slice(&self.operation_epoch);
            bytes[OPERATION_EPOCH_BYTES] |= 1;
            let candidate = OperationId::from_bytes(bytes);
            if !registry.entries.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    async fn lookup(&self, operation_id: OperationId) -> Option<OperationLookup> {
        let mut state = self.state.lock().await;
        state.prune(Instant::now(), self.retention);
        state
            .entries
            .get(&operation_id)
            .map(OperationRecord::lookup)
    }

    fn belongs_to_current_epoch(&self, operation_id: OperationId) -> bool {
        operation_id.as_bytes()[..OPERATION_EPOCH_BYTES] == self.operation_epoch
    }
}

impl OperationServiceOwner {
    pub fn new(
        daemon_instance_id: DaemonInstanceId,
        coordinator: ReindexCoordinator,
        lifecycle_admission: AdmissionGate,
    ) -> Self {
        Self {
            service: OperationService::new(daemon_instance_id, coordinator, lifecycle_admission),
            draining: None,
        }
    }

    #[must_use]
    pub fn service(&self) -> OperationService {
        self.service.clone()
    }

    pub async fn shutdown(&mut self) -> Result<(), anyhow::Error> {
        self.service.lifecycle_admission.begin_draining().await;
        if self.draining.is_none() {
            let _admission = self.service.admission_gate.lock().await;
            let mut tasks = self.service.tasks.lock().await;
            tasks.accepting = false;
            self.draining = Some(std::mem::replace(&mut tasks.tasks, JoinSet::new()));
        }
        let draining = self
            .draining
            .as_mut()
            .expect("operation shutdown initialized its drain set");
        let mut first_failure = None;
        while let Some(result) = draining.join_next().await {
            if let Err(error) = result
                && first_failure.is_none()
            {
                first_failure = Some(crate::truncate_utf8(error.to_string(), 4 * 1024));
            }
        }
        self.draining = None;
        if first_failure.is_none() {
            first_failure = self
                .service
                .state
                .lock()
                .await
                .completion_task_failure
                .clone();
        }
        match first_failure {
            Some(error) => Err(anyhow::anyhow!("operation completion task failed: {error}")),
            None => Ok(()),
        }
    }
}

enum OperationLookup {
    Entry(Arc<OperationEntry>),
    Snapshot(Box<OperationSnapshot>),
}

impl OperationRecord {
    fn lookup(&self) -> OperationLookup {
        match self {
            Self::Active(entry) | Self::Terminal(entry) => {
                OperationLookup::Entry(Arc::clone(entry))
            }
            Self::Expired(status) => OperationLookup::Snapshot(status.clone()),
        }
    }
}

async fn wait_for_terminal(entry: &OperationEntry) -> OperationSnapshot {
    loop {
        let notified = entry.changed.notified();
        tokio::pin!(notified);
        let _already_notified = notified.as_mut().enable();
        let status = entry.status.read().await.clone();
        if status.state.is_terminal() {
            return status;
        }
        notified.await;
    }
}

async fn complete_operation(
    entry: Arc<OperationEntry>,
    mut observation: ReindexObservation,
    operation_id: OperationId,
    registry: Arc<Mutex<RegistryState>>,
    retention: OperationRetentionPolicy,
) {
    loop {
        match observation.next_progress().await {
            ReindexObservationProgress::Coalesced => {
                let mut status = entry.status.write().await;
                advance_active_state(&mut status, ReindexOperationState::Coalesced);
            }
            ReindexObservationProgress::Running => {
                let mut status = entry.status.write().await;
                advance_active_state(&mut status, ReindexOperationState::Running);
            }
            ReindexObservationProgress::Cancelled => {
                let mut status = entry.status.write().await;
                status.state = ReindexOperationState::Cancelled;
                status.completion = None;
                status.status = None;
                status.failure = None;
                drop(status);
                registry
                    .lock()
                    .await
                    .mark_terminal(operation_id, Instant::now(), retention);
                entry.changed.notify_waiters();
                return;
            }
            ReindexObservationProgress::Terminal(result) => {
                let terminal = match *result {
                    Ok(completion) => OperationSnapshot {
                        origin: Some(entry.origin),
                        operation_id,
                        state: ReindexOperationState::Succeeded,
                        admission: Some(completion.admission),
                        completion: Some(completion.terminal),
                        status: Some(completion.status),
                        failure: None,
                    },
                    Err(error) => failed_snapshot(entry.origin, operation_id, error),
                };
                *entry.status.write().await = terminal;
                registry
                    .lock()
                    .await
                    .mark_terminal(operation_id, Instant::now(), retention);
                entry.changed.notify_waiters();
                return;
            }
        }
        entry.changed.notify_waiters();
    }
}

fn failed_snapshot(
    origin: OperationOrigin,
    operation_id: OperationId,
    error: CoordinatorError,
) -> OperationSnapshot {
    let (admission, failure) = match error {
        CoordinatorError::ExecutionFailed {
            admission,
            scope,
            message,
        } => (
            Some(*admission),
            OperationFailure::Execution { scope, message },
        ),
        CoordinatorError::CompletionChannelClosed { admission } => {
            (Some(*admission), OperationFailure::CompletionChannelClosed)
        }
        other => unreachable!("unexpected terminal coordinator error: {other}"),
    };
    OperationSnapshot {
        origin: Some(origin),
        operation_id,
        state: ReindexOperationState::Failed,
        admission,
        completion: None,
        status: None,
        failure: Some(failure),
    }
}

fn advance_active_state(status: &mut OperationSnapshot, observed: ReindexOperationState) {
    let allowed = matches!(
        (status.state, observed),
        (
            ReindexOperationState::Queued,
            ReindexOperationState::Coalesced | ReindexOperationState::Running
        ) | (
            ReindexOperationState::Coalesced,
            ReindexOperationState::Running
        )
    );
    if allowed {
        status.state = observed;
    }
}

impl RegistryState {
    fn mark_terminal(
        &mut self,
        operation_id: OperationId,
        now: Instant,
        retention: OperationRetentionPolicy,
    ) {
        let Some(record) = self.entries.get_mut(&operation_id) else {
            return;
        };
        let OperationRecord::Active(entry) = record else {
            return;
        };
        *record = OperationRecord::Terminal(Arc::clone(entry));
        self.active_count = self.active_count.saturating_sub(1);
        self.terminal_order.push_back((now, operation_id));
        self.prune(now, retention);
    }

    fn prune(&mut self, now: Instant, retention: OperationRetentionPolicy) {
        while self.terminal_order.len() > retention.maximum_terminal
            || self.terminal_order.front().is_some_and(|(finished, _)| {
                now.saturating_duration_since(*finished) >= retention.terminal_retention
            })
        {
            let Some((finished, operation_id)) = self.terminal_order.pop_front() else {
                break;
            };
            let origin = match self.entries.get(&operation_id) {
                Some(OperationRecord::Terminal(entry)) => Some(entry.origin),
                _ => continue,
            };
            self.entries.insert(
                operation_id,
                OperationRecord::Expired(Box::new(OperationSnapshot::terminal_marker(
                    origin,
                    operation_id,
                    ReindexOperationState::Expired,
                ))),
            );
            let retained_until = finished
                .checked_add(retention.terminal_retention)
                .unwrap_or(finished);
            self.expired_order
                .push_back((retained_until.min(now), operation_id));
        }

        while self.expired_order.len() > retention.maximum_expired
            || self.expired_order.front().is_some_and(|(expired, _)| {
                now.saturating_duration_since(*expired) >= retention.expired_retention
            })
        {
            let Some((_, operation_id)) = self.expired_order.pop_front() else {
                break;
            };
            if matches!(
                self.entries.get(&operation_id),
                Some(OperationRecord::Expired(_))
            ) {
                let origin = match self.entries.remove(&operation_id) {
                    Some(OperationRecord::Expired(status)) => status.origin,
                    _ => None,
                };
                self.idempotency
                    .retain(|_, retained| retained.operation_id != operation_id);
                if let Some(origin) = origin.and_then(OperationOrigin::background) {
                    let remove_origin =
                        self.background_by_origin
                            .get_mut(&origin)
                            .is_some_and(|operations| {
                                operations.retain(|retained| *retained != operation_id);
                                operations.is_empty()
                            });
                    if remove_origin {
                        self.background_by_origin.remove(&origin);
                    }
                }
            }
        }
    }
}

/// Owns mandatory full re-analysis until persisted semantics and configuration are current.
#[must_use = "semantic upgrade supervision must be joined before daemon leases release"]
pub struct SemanticUpgradeRuntime {
    first_admission: Option<oneshot::Receiver<Result<OperationId, String>>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl SemanticUpgradeRuntime {
    pub fn start(required: bool, operations: OperationService) -> Self {
        let (shutdown, shutdown_receiver) = watch::channel(false);
        if !required {
            return Self {
                first_admission: None,
                shutdown,
                task: None,
            };
        }
        let (first_sender, first_admission) = oneshot::channel();
        let task = tokio::spawn(run_semantic_upgrade(
            operations,
            shutdown_receiver,
            first_sender,
        ));
        Self {
            first_admission: Some(first_admission),
            shutdown,
            task: Some(task),
        }
    }

    pub async fn ensure_first_admission(&mut self) -> Result<Option<OperationId>, anyhow::Error> {
        let Some(first_admission) = self.first_admission.take() else {
            return Ok(None);
        };
        match first_admission.await {
            Ok(Ok(operation_id)) => Ok(Some(operation_id)),
            Ok(Err(message)) => Err(anyhow::anyhow!(message)),
            Err(_) => Err(anyhow::anyhow!(
                "semantic upgrade supervisor terminated before first admission"
            )),
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), anyhow::Error> {
        self.shutdown.send_replace(true);
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await.map_err(|error| {
            let message = crate::truncate_utf8(error.to_string(), 4 * 1024);
            anyhow::anyhow!("semantic upgrade task failed: {message}")
        })
    }
}

impl Drop for SemanticUpgradeRuntime {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_semantic_upgrade(
    operations: OperationService,
    mut shutdown: watch::Receiver<bool>,
    first_admission: oneshot::Sender<Result<OperationId, String>>,
) {
    let mut first_admission = Some(first_admission);
    let mut backoff = INITIAL_SEMANTIC_RETRY_BACKOFF;
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        let admitted = operations
            .admit(
                OperationOrigin::SemanticUpgrade,
                FilesystemReindexIntent::full(),
                None,
            )
            .await;
        let operation_id = match admitted {
            Ok(status) => {
                if let Some(sender) = first_admission.take() {
                    let _receiver_was_dropped = sender.send(Ok(status.operation_id));
                }
                status.operation_id
            }
            Err(error) if error.is_retryable_admission() => {
                if wait_for_retry(&mut shutdown, backoff).await {
                    return;
                }
                backoff = backoff
                    .saturating_mul(2)
                    .min(MAXIMUM_SEMANTIC_RETRY_BACKOFF);
                continue;
            }
            Err(error) => {
                if let Some(sender) = first_admission.take() {
                    let _receiver_was_dropped = sender.send(Err(format!(
                        "mandatory semantic upgrade admission failed: {error}"
                    )));
                    return;
                }
                if wait_for_retry(&mut shutdown, backoff).await {
                    return;
                }
                backoff = backoff
                    .saturating_mul(2)
                    .min(MAXIMUM_SEMANTIC_RETRY_BACKOFF);
                continue;
            }
        };

        let terminal = tokio::select! {
            biased;
            () = shutdown_requested(&mut shutdown) => return,
            terminal = operations.wait_until_terminal(operation_id) => terminal,
        };
        if terminal.is_ok_and(|status| {
            status.state == ReindexOperationState::Succeeded && status.semantics_are_current()
        }) {
            return;
        }
        if wait_for_retry(&mut shutdown, backoff).await {
            return;
        }
        backoff = backoff
            .saturating_mul(2)
            .min(MAXIMUM_SEMANTIC_RETRY_BACKOFF);
    }
}

async fn wait_for_retry(shutdown: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    tokio::select! {
        biased;
        () = shutdown_requested(shutdown) => true,
        () = tokio::time::sleep(duration) => false,
    }
}

async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() || shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Notify;
    use tokio::time::Instant;
    use unity_asset_search_index::{FilesystemReindexIntent, IndexPaths, ProjectPathSpace};
    use unity_asset_search_protocol::{
        BackgroundReindexOperation, BackgroundReindexOrigin, DaemonInstanceId,
        ReindexOperationState,
    };

    use super::{
        INITIAL_SEMANTIC_RETRY_BACKOFF, OperationError, OperationFailure, OperationOrigin,
        OperationRetentionPolicy, OperationService, OperationServiceOwner, SemanticUpgradeRuntime,
    };
    use crate::coordinator::{
        ReindexCoordinatorConfig, ReindexCoordinatorRuntime, ReindexScopeKind,
    };
    use crate::lifecycle::AdmissionGate;
    use crate::watcher::{MaintenanceRuntime, TimerLifecycle};

    struct CoordinatorFixture {
        _project: tempfile::TempDir,
        runtime: ReindexCoordinatorRuntime,
    }

    impl CoordinatorFixture {
        fn pending() -> Self {
            Self::with_executor(|_intent| async move { std::future::pending().await })
        }

        fn with_executor<F, Fut>(executor: F) -> Self
        where
            F: Fn(FilesystemReindexIntent) -> Fut + Send + Sync + 'static,
            Fut: std::future::Future<Output = anyhow::Result<crate::coordinator::ReindexExecution>>
                + Send
                + 'static,
        {
            let project = crate::secure_test_tempdir();
            let assets = project.path().join("Assets");
            std::fs::create_dir(&assets).unwrap();
            let paths = IndexPaths::for_project(
                project.path().to_path_buf(),
                Some(project.path().join(".unity-asset-index")),
                Some(vec![assets]),
            )
            .unwrap();
            let config = ReindexCoordinatorConfig::new(paths.project_path_space().clone())
                .with_debounce(Duration::from_secs(60))
                .with_max_debounce(Duration::from_secs(60));
            let runtime = ReindexCoordinatorRuntime::start(config, executor).unwrap();
            Self {
                _project: project,
                runtime,
            }
        }

        fn service(&self, instance: u8) -> OperationService {
            OperationService::new(
                DaemonInstanceId::from_bytes([instance; 16]),
                self.runtime.coordinator(),
                AdmissionGate::default(),
            )
        }

        fn path_space(&self) -> ProjectPathSpace {
            self.runtime.coordinator().project_path_space().clone()
        }
    }

    async fn background_operation(
        service: &OperationService,
        origin: BackgroundReindexOrigin,
    ) -> Option<BackgroundReindexOperation> {
        service
            .background_operations()
            .await
            .unwrap()
            .into_iter()
            .find(|operation| operation.origin == origin)
    }

    #[tokio::test(start_paused = true)]
    async fn timer_admission_returns_real_executor_failure_evidence() {
        let mut fixture = CoordinatorFixture::with_executor(|_intent| async move {
            anyhow::bail!("timer build failed")
        });
        let service = fixture.service(14);

        let terminal = service.admit_timer_and_wait().await.unwrap();

        assert_eq!(terminal.state, ReindexOperationState::Failed);
        assert_eq!(
            terminal.failure,
            Some(OperationFailure::Execution {
                scope: ReindexScopeKind::Reconcile,
                message: "timer build failed".to_owned(),
            })
        );
        fixture.runtime.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn maintenance_shutdown_stops_waiting_while_operation_owners_drain_execution() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let executor_started = Arc::clone(&started);
        let executor_release = Arc::clone(&release);
        let fixture = CoordinatorFixture::with_executor(move |_intent| {
            let started = Arc::clone(&executor_started);
            let release = Arc::clone(&executor_release);
            async move {
                started.notify_one();
                release.notified().await;
                anyhow::bail!("timer build failed after maintenance stopped")
            }
        });
        let admission = AdmissionGate::default();
        let mut owner = OperationServiceOwner::new(
            DaemonInstanceId::from_bytes([15; 16]),
            fixture.runtime.coordinator(),
            admission,
        );
        let service = owner.service();
        let mut maintenance =
            MaintenanceRuntime::start(service.clone(), None, Some(Duration::from_secs(1)));
        let maintenance_status = maintenance.handle();

        tokio::time::advance(Duration::from_secs(61)).await;
        started.notified().await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let active = background_operation(&service, BackgroundReindexOrigin::Timer)
            .await
            .unwrap();
        assert_eq!(active.state, ReindexOperationState::Running);

        maintenance.shutdown().await.unwrap();
        assert_eq!(
            maintenance_status.snapshot().await.timer,
            TimerLifecycle::Stopped
        );

        let CoordinatorFixture {
            _project,
            mut runtime,
        } = fixture;
        let coordinator_shutdown = tokio::spawn(async move { runtime.shutdown().await });
        tokio::task::yield_now().await;
        assert!(!coordinator_shutdown.is_finished());

        release.notify_one();
        coordinator_shutdown.await.unwrap().unwrap();
        owner.shutdown().await.unwrap();
        let terminal = service.status(active.operation_id).await.unwrap();
        assert_eq!(terminal.state, ReindexOperationState::Failed);
        assert_eq!(
            terminal.failure,
            Some(OperationFailure::Execution {
                scope: ReindexScopeKind::Reconcile,
                message: "timer build failed after maintenance stopped".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn every_internal_origin_retains_a_queryable_operation_id() {
        let fixture = CoordinatorFixture::pending();
        let service = fixture.service(1);

        let startup = service
            .admit(
                OperationOrigin::Startup,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        let watcher = service
            .admit(
                OperationOrigin::Watcher,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        let overflow = service.admit_watcher_overflow().await.unwrap();
        let timer = service
            .admit(
                OperationOrigin::Timer,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        let semantic = service
            .admit(
                OperationOrigin::SemanticUpgrade,
                FilesystemReindexIntent::full(),
                None,
            )
            .await
            .unwrap();
        service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            service
                .background_operations()
                .await
                .unwrap()
                .into_iter()
                .map(|operation| (operation.origin, operation.operation_id))
                .collect::<Vec<_>>(),
            vec![
                (BackgroundReindexOrigin::Startup, startup.operation_id),
                (BackgroundReindexOrigin::Watcher, watcher.operation_id),
                (
                    BackgroundReindexOrigin::WatcherOverflow,
                    overflow.operation_id,
                ),
                (BackgroundReindexOrigin::Timer, timer.operation_id),
                (
                    BackgroundReindexOrigin::SemanticUpgrade,
                    semantic.operation_id,
                ),
            ]
        );
        let coordinator = service.coordinator_snapshot().await;
        assert_eq!(coordinator.watcher_overflows, 1);
        assert_eq!(coordinator.admissions.startup, 1);
        assert_eq!(coordinator.admissions.watcher, 2);
        assert_eq!(coordinator.admissions.timer, 1);
    }

    #[tokio::test]
    async fn background_summary_replaces_an_origin_without_exposing_ipc_operations() {
        let fixture = CoordinatorFixture::pending();
        let service = fixture.service(10);
        let first = service
            .admit(
                OperationOrigin::Watcher,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        let second = service
            .admit(
                OperationOrigin::Watcher,
                FilesystemReindexIntent::full(),
                None,
            )
            .await
            .unwrap();
        service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();

        let operations = service.background_operations().await.unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].origin, BackgroundReindexOrigin::Watcher);
        assert_eq!(operations[0].operation_id, second.operation_id);
        assert_ne!(operations[0].operation_id, first.operation_id);
    }

    #[tokio::test]
    async fn daemon_owned_operation_cannot_be_cancelled_by_a_client() {
        let fixture = CoordinatorFixture::pending();
        let service = fixture.service(11);
        let operation = service
            .admit(
                OperationOrigin::Watcher,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();

        assert!(matches!(
            service.cancel(operation.operation_id).await,
            Err(OperationError::ControlForbidden {
                origin: OperationOrigin::Watcher
            })
        ));
        assert_eq!(
            background_operation(&service, BackgroundReindexOrigin::Watcher)
                .await
                .unwrap()
                .operation_id,
            operation.operation_id
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expired_daemon_owned_operation_remains_discoverable_and_not_client_cancelable() {
        let fixture = CoordinatorFixture::pending();
        let retention = OperationRetentionPolicy {
            maximum_active: 4,
            maximum_client_active: 4,
            maximum_terminal: 4,
            maximum_expired: 4,
            terminal_retention: Duration::from_secs(1),
            expired_retention: Duration::from_secs(1),
        };
        let service = OperationService::with_retention(
            DaemonInstanceId::from_bytes([13; 16]),
            fixture.runtime.coordinator(),
            AdmissionGate::default(),
            retention,
        );
        let operation = service
            .admit(
                OperationOrigin::Watcher,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        service
            .state
            .lock()
            .await
            .mark_terminal(operation.operation_id, Instant::now(), retention);

        tokio::time::advance(Duration::from_secs(1)).await;
        let expired = background_operation(&service, BackgroundReindexOrigin::Watcher)
            .await
            .unwrap();
        assert_eq!(expired.operation_id, operation.operation_id);
        assert_eq!(expired.state, ReindexOperationState::Expired);
        assert!(matches!(
            service.cancel(operation.operation_id).await,
            Err(OperationError::ControlForbidden {
                origin: OperationOrigin::Watcher
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn background_summary_falls_back_to_an_older_retained_operation() {
        let fixture = CoordinatorFixture::pending();
        let retention = OperationRetentionPolicy {
            maximum_active: 4,
            maximum_client_active: 4,
            maximum_terminal: 4,
            maximum_expired: 1,
            terminal_retention: Duration::from_secs(1),
            expired_retention: Duration::from_secs(1),
        };
        let service = OperationService::with_retention(
            DaemonInstanceId::from_bytes([12; 16]),
            fixture.runtime.coordinator(),
            AdmissionGate::default(),
            retention,
        );
        let older = service
            .admit(
                OperationOrigin::Watcher,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        let newer = service
            .admit(
                OperationOrigin::Watcher,
                FilesystemReindexIntent::full(),
                None,
            )
            .await
            .unwrap();
        {
            let mut registry = service.state.lock().await;
            let now = Instant::now();
            registry.mark_terminal(newer.operation_id, now, retention);
        }

        tokio::time::advance(Duration::from_secs(2)).await;
        let retained = background_operation(&service, BackgroundReindexOrigin::Watcher)
            .await
            .unwrap();
        assert_eq!(retained.operation_id, older.operation_id);
        assert_ne!(retained.operation_id, newer.operation_id);
    }

    #[tokio::test]
    async fn internal_admission_uses_capacity_reserved_from_clients() {
        let fixture = CoordinatorFixture::pending();
        let retention = OperationRetentionPolicy {
            maximum_active: 3,
            maximum_client_active: 2,
            maximum_terminal: 4,
            maximum_expired: 4,
            terminal_retention: Duration::from_secs(60),
            expired_retention: Duration::from_secs(60),
        };
        let service = OperationService::with_retention(
            DaemonInstanceId::from_bytes([2; 16]),
            fixture.runtime.coordinator(),
            AdmissionGate::default(),
            retention,
        );

        for _ in 0..2 {
            service
                .admit(
                    OperationOrigin::Ipc,
                    FilesystemReindexIntent::reconcile(),
                    None,
                )
                .await
                .unwrap();
        }
        assert!(matches!(
            service
                .admit(
                    OperationOrigin::Ipc,
                    FilesystemReindexIntent::reconcile(),
                    None,
                )
                .await,
            Err(OperationError::Saturated { maximum: 2 })
        ));

        service.admit_watcher_overflow().await.unwrap();
        assert!(matches!(
            service
                .admit(
                    OperationOrigin::Timer,
                    FilesystemReindexIntent::reconcile(),
                    None,
                )
                .await,
            Err(OperationError::Saturated { maximum: 3 })
        ));
    }

    #[tokio::test]
    async fn idempotency_key_is_bound_to_the_normalized_intent() {
        let fixture = CoordinatorFixture::pending();
        let service = fixture.service(3);
        let first = service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::full(),
                Some("same-key".to_owned()),
            )
            .await
            .unwrap();
        let repeated = service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::full(),
                Some("same-key".to_owned()),
            )
            .await
            .unwrap();
        assert_eq!(repeated.operation_id, first.operation_id);

        assert!(matches!(
            service
                .admit(
                    OperationOrigin::Ipc,
                    FilesystemReindexIntent::reconcile(),
                    Some("same-key".to_owned()),
                )
                .await,
            Err(OperationError::IdempotencyConflict { operation_id })
                if operation_id == first.operation_id
        ));
    }

    #[tokio::test]
    async fn queued_cancellation_is_terminal_but_coalesced_work_is_shared() {
        let fixture = CoordinatorFixture::pending();
        let service = fixture.service(7);
        let first = service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        let second = service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();

        let shared = service.cancel(second.operation_id).await.unwrap();
        assert!(!shared.cancelled);
        assert_eq!(shared.state, ReindexOperationState::Coalesced);
        let exclusive = service.cancel(first.operation_id).await.unwrap();
        assert!(!exclusive.cancelled);
        assert_eq!(exclusive.state, ReindexOperationState::Coalesced);
    }

    #[tokio::test]
    async fn one_exclusive_queued_operation_can_be_cancelled() {
        let fixture = CoordinatorFixture::pending();
        let service = fixture.service(8);
        let operation = service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();

        let cancellation = service.cancel(operation.operation_id).await.unwrap();
        assert!(cancellation.cancelled);
        assert_eq!(cancellation.state, ReindexOperationState::Cancelled);
        assert_eq!(
            service.status(operation.operation_id).await.unwrap().state,
            ReindexOperationState::Cancelled
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_retention_distinguishes_expired_lost_and_not_found() {
        let fixture = CoordinatorFixture::pending();
        let retention = OperationRetentionPolicy {
            maximum_active: 4,
            maximum_client_active: 4,
            maximum_terminal: 4,
            maximum_expired: 4,
            terminal_retention: Duration::from_secs(1),
            expired_retention: Duration::from_secs(1),
        };
        let service = OperationService::with_retention(
            DaemonInstanceId::from_bytes([9; 16]),
            fixture.runtime.coordinator(),
            AdmissionGate::default(),
            retention,
        );
        let operation = service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        assert!(
            service
                .cancel(operation.operation_id)
                .await
                .unwrap()
                .cancelled
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        let expired = service.status(operation.operation_id).await.unwrap();
        assert_eq!(expired.state, ReindexOperationState::Expired);
        assert_eq!(expired.origin, Some(OperationOrigin::Ipc));

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(
            service.status(operation.operation_id).await,
            Err(OperationError::NotFound)
        ));
        let prior_daemon = service
            .status(unity_asset_search_protocol::OperationId::from_bytes(
                [3; 16],
            ))
            .await
            .unwrap();
        assert_eq!(prior_daemon.state, ReindexOperationState::Lost);
        assert_eq!(prior_daemon.origin, None);
    }

    #[tokio::test]
    async fn shared_draining_gate_rejects_internal_and_client_operations() {
        let fixture = CoordinatorFixture::pending();
        let admission = AdmissionGate::default();
        let service = OperationService::new(
            DaemonInstanceId::from_bytes([4; 16]),
            fixture.runtime.coordinator(),
            admission.clone(),
        );
        admission.close();

        for origin in [
            OperationOrigin::Watcher,
            OperationOrigin::Timer,
            OperationOrigin::Ipc,
        ] {
            assert!(matches!(
                service
                    .admit(origin, FilesystemReindexIntent::reconcile(), None)
                    .await,
                Err(OperationError::Draining)
            ));
        }
        assert!(matches!(
            service.admit_watcher_overflow().await,
            Err(OperationError::Draining)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn semantic_upgrade_is_mandatory_observable_and_retried() {
        let executions = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&executions);
        let fixture = CoordinatorFixture::with_executor(move |_intent| {
            let observed = Arc::clone(&observed);
            async move {
                let attempt = observed.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("first semantic upgrade attempt failed");
                }
                std::future::pending().await
            }
        });
        let service = fixture.service(5);
        let mut semantic = SemanticUpgradeRuntime::start(true, service.clone());

        let first_id = semantic.ensure_first_admission().await.unwrap().unwrap();
        assert_eq!(
            background_operation(&service, BackgroundReindexOrigin::SemanticUpgrade)
                .await
                .unwrap()
                .operation_id,
            first_id
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(INITIAL_SEMANTIC_RETRY_BACKOFF).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let retried = background_operation(&service, BackgroundReindexOrigin::SemanticUpgrade)
            .await
            .unwrap();
        assert_ne!(retried.operation_id, first_id);
        assert!(matches!(
            retried.state,
            ReindexOperationState::Queued
                | ReindexOperationState::Coalesced
                | ReindexOperationState::Running
        ));

        semantic.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn semantic_upgrade_waits_through_retryable_startup_saturation() {
        let fixture = CoordinatorFixture::pending();
        let retention = OperationRetentionPolicy {
            maximum_active: 1,
            maximum_client_active: 1,
            maximum_terminal: 4,
            maximum_expired: 4,
            terminal_retention: Duration::from_secs(60),
            expired_retention: Duration::from_secs(60),
        };
        let service = OperationService::with_retention(
            DaemonInstanceId::from_bytes([6; 16]),
            fixture.runtime.coordinator(),
            AdmissionGate::default(),
            retention,
        );
        let blocker = service
            .admit(
                OperationOrigin::Ipc,
                FilesystemReindexIntent::reconcile(),
                None,
            )
            .await
            .unwrap();
        let mut semantic = SemanticUpgradeRuntime::start(true, service.clone());
        let publication_allowed = Arc::new(AtomicBool::new(false));
        let task_publication_allowed = Arc::clone(&publication_allowed);
        let admission = tokio::spawn(async move {
            let result = semantic.ensure_first_admission().await;
            if result.is_ok() {
                task_publication_allowed.store(true, Ordering::Release);
            }
            (semantic, result)
        });

        tokio::task::yield_now().await;
        assert!(!admission.is_finished());
        assert!(!publication_allowed.load(Ordering::Acquire));
        tokio::time::advance(INITIAL_SEMANTIC_RETRY_BACKOFF).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(!admission.is_finished());
        assert!(!publication_allowed.load(Ordering::Acquire));

        let cancellation = service.cancel(blocker.operation_id).await.unwrap();
        assert!(cancellation.cancelled);
        tokio::time::advance(INITIAL_SEMANTIC_RETRY_BACKOFF.saturating_mul(2)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        let (mut semantic, admitted) = admission.await.unwrap();
        let admitted = admitted.unwrap().unwrap();
        assert!(publication_allowed.load(Ordering::Acquire));
        assert_eq!(
            background_operation(&service, BackgroundReindexOrigin::SemanticUpgrade)
                .await
                .unwrap()
                .operation_id,
            admitted
        );
        semantic.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fixture_path_space_remains_project_bound() {
        let fixture = CoordinatorFixture::pending();
        assert_eq!(
            fixture.path_space().project_id(),
            fixture
                .runtime
                .coordinator()
                .project_path_space()
                .project_id()
        );
    }
}
