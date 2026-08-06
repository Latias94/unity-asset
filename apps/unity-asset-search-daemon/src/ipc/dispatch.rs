use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::{Mutex, Notify, RwLock, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};
use unity_asset_search_index::{
    FilesystemReindexIntent as IndexReindexIntent, FilesystemReindexScope as IndexReindexScope,
    ProjectPathError, ProjectPathSpace, SearchIndex, SearchRequest as IndexSearchRequest,
};
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, CapabilitiesResponse, DaemonInstanceId, DaemonLifecycleState,
    DaemonLifecycleStatus, FilesystemReindexIntent, FilesystemReindexScope, FreshnessMaintenance,
    OperationId, QueryPolicyId, ReconcileLifecycle, ReindexCancelResponse, ReindexDisposition,
    ReindexOperationState, ReindexOperationStatus, RequestOperation, ResponseOperation,
    SearchCapabilities, ShutdownResponse, TimerLifecycleState, TimerStatus, WatcherLifecycleState,
    WatcherStatus,
};

use crate::coordinator::{
    CoordinatorError, ReindexCancellation, ReindexCancellationOutcome, ReindexCoordinator,
    ReindexObservation, ReindexObservationProgress, ReindexSource,
};
use crate::lifecycle::{AdmissionGate, BlockingTaskError, BlockingTaskHandle};
use crate::watcher::{
    MaintenanceHandle, MaintenanceSnapshot, TimerLifecycle as InternalTimerLifecycle,
    WatcherLifecycle as InternalWatcherLifecycle,
};

const MAX_ACTIVE_OPERATIONS: usize = 256;
const MAX_RETAINED_TERMINAL_OPERATIONS: usize = 256;
const MAX_RETAINED_EXPIRED_OPERATIONS: usize = 256;
const TERMINAL_OPERATION_RETENTION: Duration = Duration::from_secs(10 * 60);
const EXPIRED_OPERATION_RETENTION: Duration = Duration::from_secs(10 * 60);
const OPERATION_EPOCH_BYTES: usize = 8;
const REFERENCE_QUERY_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const REFERENCE_QUERY_BUDGET_ENTRIES: u64 = 2 * 1024 * 1024;
const REFERENCE_QUERY_BUDGET_MEMBERS: u64 = 2 * 1024 * 1024;
const REFERENCE_QUERY_BUDGET_DEPTH: u32 = 32;

#[derive(Clone)]
pub struct Dispatcher {
    inner: Arc<DispatcherInner>,
}

struct DispatcherInner {
    index: SearchIndex,
    query_policy_id: QueryPolicyId,
    operations: OperationRegistry,
    admission: AdmissionGate,
    blocking_tasks: BlockingTaskHandle,
    maintenance: MaintenanceHandle,
    shutdown: watch::Sender<Option<Instant>>,
}

#[derive(Clone)]
pub(crate) struct DispatcherShutdown {
    shutdown: watch::Sender<Option<Instant>>,
    admission: AdmissionGate,
}

pub struct DispatchResult {
    pub response: Result<ResponseOperation, ApiError>,
    pub shutdown_after_response: Option<Instant>,
}

impl Dispatcher {
    pub fn new(
        index: SearchIndex,
        blocking_tasks: BlockingTaskHandle,
        operations: OperationRegistry,
        admission: AdmissionGate,
        maintenance: MaintenanceHandle,
    ) -> Self {
        let query_policy_id = operations.query_policy_id();
        let (shutdown, _) = watch::channel(None);
        Self {
            inner: Arc::new(DispatcherInner {
                index,
                query_policy_id,
                operations,
                admission,
                blocking_tasks,
                maintenance,
                shutdown,
            }),
        }
    }

    #[must_use]
    pub fn query_policy_id(&self) -> QueryPolicyId {
        self.inner.query_policy_id
    }

    pub fn subscribe_shutdown(&self) -> watch::Receiver<Option<Instant>> {
        self.inner.shutdown.subscribe()
    }

    pub(crate) fn shutdown_handle(&self) -> DispatcherShutdown {
        DispatcherShutdown {
            shutdown: self.inner.shutdown.clone(),
            admission: self.inner.admission.clone(),
        }
    }

    pub(crate) fn requested_shutdown_deadline(&self) -> Option<Instant> {
        *self.inner.shutdown.borrow()
    }

    pub fn begin_shutdown_at(&self, deadline: Instant) {
        self.shutdown_handle().begin_shutdown_at(deadline);
    }

    pub async fn begin_draining(&self) {
        self.inner.admission.begin_draining().await;
    }

    pub async fn dispatch(&self, operation: RequestOperation) -> DispatchResult {
        let shutdown_after_response = requested_shutdown_deadline(&operation);
        if matches!(operation, RequestOperation::Shutdown(_)) {
            self.inner.admission.close();
        } else if requires_lifecycle_admission(operation.kind())
            && self.inner.admission.admit().await.is_none()
        {
            return DispatchResult {
                response: Err(ApiError::new(
                    ApiErrorCode::NotReady,
                    "daemon is draining and no longer accepts this operation",
                    false,
                )
                .with_detail("lifecycle", "draining")
                .with_query_policy(self.inner.query_policy_id)),
                shutdown_after_response: None,
            };
        }
        let response = match operation {
            RequestOperation::Capabilities(_) => {
                Ok(ResponseOperation::Capabilities(CapabilitiesResponse {
                    daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
                    capabilities: SearchCapabilities::current(),
                }))
            }
            RequestOperation::Status(_) => self.status().await.map(ResponseOperation::Status),
            RequestOperation::Search(request) => {
                let index = self.inner.index.clone();
                blocking_index(&self.inner.blocking_tasks, move || {
                    index.search(IndexSearchRequest::new(
                        request.query,
                        request.limit as usize,
                    ))
                })
                .await
                .map(ResponseOperation::Search)
            }
            RequestOperation::Suggest(request) => {
                let index = self.inner.index.clone();
                blocking_index(&self.inner.blocking_tasks, move || {
                    index.suggest(&request.prefix, request.limit as usize)
                })
                .await
                .map(ResponseOperation::Suggest)
            }
            RequestOperation::References(request) => match reference_query_budget() {
                Ok(mut budget) => {
                    let index = self.inner.index.clone();
                    blocking_index(&self.inner.blocking_tasks, move || {
                        index.references(request, &mut budget)
                    })
                    .await
                    .map(ResponseOperation::References)
                }
                Err(_) => Err(ApiError::new(
                    ApiErrorCode::Internal,
                    "reference query budget configuration is invalid",
                    false,
                )),
            },
            RequestOperation::ReindexAdmit(request) => self
                .inner
                .operations
                .admit_client(request.intent, request.idempotency_key)
                .await
                .map(ResponseOperation::ReindexAdmit),
            RequestOperation::ReindexStatus(request) => self
                .inner
                .operations
                .status(request.operation_id, self.inner.query_policy_id)
                .await
                .map(ResponseOperation::ReindexStatus),
            RequestOperation::ReindexWait(request) => self
                .inner
                .operations
                .wait(
                    request.operation_id,
                    Duration::from_millis(u64::from(request.timeout_ms)),
                    self.inner.query_policy_id,
                )
                .await
                .map(ResponseOperation::ReindexWait),
            RequestOperation::ReindexCancel(request) => self
                .inner
                .operations
                .cancel(request.operation_id, self.inner.query_policy_id)
                .await
                .map(ResponseOperation::ReindexCancel),
            RequestOperation::Shutdown(_) => Ok(ResponseOperation::Shutdown(ShutdownResponse {
                accepted: true,
            })),
        };
        DispatchResult {
            response,
            shutdown_after_response,
        }
    }

    async fn status(&self) -> Result<unity_asset_search_protocol::StatusResponse, ApiError> {
        let index = self.inner.index.clone();
        let mut status = blocking_index(&self.inner.blocking_tasks, move || index.status()).await?;
        let coordinator = self.inner.operations.coordinator_snapshot().await;
        let maintenance = self.inner.maintenance.snapshot().await;
        status.daemon = daemon_lifecycle_status(
            self.inner.admission.is_draining().await,
            &status.daemon,
            &coordinator,
            maintenance,
        );
        Ok(status)
    }
}

impl DispatcherShutdown {
    pub(crate) fn begin_shutdown_at(&self, deadline: Instant) {
        self.admission.close();
        self.shutdown
            .send_if_modified(|current| tighten_shutdown(current, deadline));
    }
}

const fn requires_lifecycle_admission(
    operation: unity_asset_search_protocol::OperationKind,
) -> bool {
    !matches!(
        operation,
        unity_asset_search_protocol::OperationKind::Capabilities
            | unity_asset_search_protocol::OperationKind::Status
            | unity_asset_search_protocol::OperationKind::ReindexStatus
            | unity_asset_search_protocol::OperationKind::ReindexAdmit
            | unity_asset_search_protocol::OperationKind::Shutdown
    )
}

fn daemon_lifecycle_status(
    draining: bool,
    index_status: &DaemonLifecycleStatus,
    coordinator: &crate::coordinator::ReindexCoordinatorSnapshot,
    maintenance: MaintenanceSnapshot,
) -> DaemonLifecycleStatus {
    let watcher = watcher_lifecycle_state(maintenance.watcher);
    let timer = timer_lifecycle_state(maintenance.timer);
    let freshness_maintenance = if matches!(watcher, WatcherLifecycleState::Disabled)
        && matches!(timer, TimerLifecycleState::Disabled)
    {
        FreshnessMaintenance::Unmanaged
    } else {
        FreshnessMaintenance::Managed
    };
    let reconcile = if coordinator.in_flight.is_some() {
        ReconcileLifecycle::Running
    } else if coordinator.pending_general.is_some() {
        ReconcileLifecycle::Queued
    } else if coordinator.last_completion_failed {
        ReconcileLifecycle::Failed
    } else {
        index_status.reconcile
    };
    DaemonLifecycleStatus {
        lifecycle: if draining {
            DaemonLifecycleState::Draining
        } else {
            DaemonLifecycleState::Serving
        },
        serving: index_status.serving,
        freshness: index_status.freshness,
        freshness_maintenance,
        reconcile,
        generation_maintenance: index_status.generation_maintenance.clone(),
        watcher: WatcherStatus {
            state: watcher,
            retry_count: maintenance.watcher_retry_count,
            last_failure: maintenance.watcher_last_failure,
            next_retry_in_ms: maintenance.watcher_next_retry_in_ms,
        },
        timer: TimerStatus {
            state: timer,
            run_count: maintenance.timer_run_count,
            last_failure: maintenance.timer_last_failure,
            next_run_in_ms: maintenance.timer_next_run_in_ms,
        },
    }
}

const fn watcher_lifecycle_state(state: InternalWatcherLifecycle) -> WatcherLifecycleState {
    match state {
        InternalWatcherLifecycle::Disabled => WatcherLifecycleState::Disabled,
        InternalWatcherLifecycle::Starting => WatcherLifecycleState::Starting,
        InternalWatcherLifecycle::Healthy => WatcherLifecycleState::Healthy,
        InternalWatcherLifecycle::Retrying => WatcherLifecycleState::Retrying,
        InternalWatcherLifecycle::Stopped => WatcherLifecycleState::Stopped,
    }
}

const fn timer_lifecycle_state(state: InternalTimerLifecycle) -> TimerLifecycleState {
    match state {
        InternalTimerLifecycle::Disabled => TimerLifecycleState::Disabled,
        InternalTimerLifecycle::Scheduled => TimerLifecycleState::Scheduled,
        InternalTimerLifecycle::Running => TimerLifecycleState::Running,
        InternalTimerLifecycle::Stopped => TimerLifecycleState::Stopped,
    }
}

fn requested_shutdown_deadline(operation: &RequestOperation) -> Option<Instant> {
    match operation {
        RequestOperation::Shutdown(request) => Some(shutdown_deadline(Duration::from_millis(
            u64::from(request.drain_timeout_ms),
        ))),
        _ => None,
    }
}

fn shutdown_deadline(timeout: Duration) -> Instant {
    Instant::now()
        .checked_add(timeout)
        .expect("bounded protocol shutdown timeout fits Tokio Instant")
}

fn tighten_shutdown(current: &mut Option<Instant>, requested: Instant) -> bool {
    if current.is_none_or(|existing| requested < existing) {
        *current = Some(requested);
        true
    } else {
        false
    }
}

async fn blocking_index<T>(
    tasks: &BlockingTaskHandle,
    operation: impl FnOnce() -> Result<T, unity_asset_search_index::SearchIndexError> + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
{
    match tasks.run(operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.into_api_error()),
        Err(BlockingTaskError::ShuttingDown) => Err(ApiError::new(
            ApiErrorCode::NotReady,
            "daemon is draining and no longer accepts blocking work",
            false,
        )
        .with_detail("lifecycle", "draining")),
        Err(error) => Err(ApiError::new(
            ApiErrorCode::Internal,
            "search index worker terminated unexpectedly",
            false,
        )
        .with_detail("cause", bounded_detail(error.to_string()))),
    }
}

fn reference_query_budget() -> Result<AssetLoadBudget, BudgetError> {
    AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: REFERENCE_QUERY_BUDGET_BYTES,
        max_depth: REFERENCE_QUERY_BUDGET_DEPTH,
        max_entries: REFERENCE_QUERY_BUDGET_ENTRIES,
        max_members: REFERENCE_QUERY_BUDGET_MEMBERS,
        ..AssetLoadLimits::default()
    })
}

#[derive(Clone)]
pub struct OperationRegistry {
    admission_gate: Arc<Mutex<()>>,
    state: Arc<Mutex<RegistryState>>,
    operation_epoch: [u8; OPERATION_EPOCH_BYTES],
    retention: OperationRetentionPolicy,
    tasks: Arc<StdMutex<OperationTaskState>>,
    coordinator: ReindexCoordinator,
    project_paths: ProjectPathSpace,
    query_policy_id: QueryPolicyId,
    lifecycle_admission: AdmissionGate,
}

struct OperationTaskState {
    accepting: bool,
    tasks: JoinSet<()>,
}

#[must_use = "operation completion tasks must be joined before daemon leases release"]
pub struct OperationRegistryOwner {
    registry: OperationRegistry,
    draining: Option<JoinSet<()>>,
}

#[derive(Default)]
struct RegistryState {
    entries: BTreeMap<OperationId, OperationRecord>,
    terminal_order: VecDeque<(Instant, OperationId)>,
    expired_order: VecDeque<(Instant, OperationId)>,
    idempotency: BTreeMap<String, IdempotencyBinding>,
    active_count: usize,
}

#[derive(Clone, Copy)]
struct IdempotencyBinding {
    operation_id: OperationId,
    intent_fingerprint: [u8; 32],
}

#[derive(Clone, Copy)]
struct OperationRetentionPolicy {
    maximum_active: usize,
    maximum_terminal: usize,
    maximum_expired: usize,
    terminal_retention: Duration,
    expired_retention: Duration,
}

impl Default for OperationRetentionPolicy {
    fn default() -> Self {
        Self {
            maximum_active: MAX_ACTIVE_OPERATIONS,
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
    Expired(Box<ReindexOperationStatus>),
}

struct OperationEntry {
    status: RwLock<ReindexOperationStatus>,
    changed: Notify,
    cancellation: ReindexCancellation,
}

impl OperationRegistry {
    fn new(
        daemon_instance_id: DaemonInstanceId,
        coordinator: ReindexCoordinator,
        query_policy_id: QueryPolicyId,
        lifecycle_admission: AdmissionGate,
    ) -> Self {
        Self::with_retention(
            daemon_instance_id,
            coordinator,
            query_policy_id,
            lifecycle_admission,
            OperationRetentionPolicy::default(),
        )
    }

    fn with_retention(
        daemon_instance_id: DaemonInstanceId,
        coordinator: ReindexCoordinator,
        query_policy_id: QueryPolicyId,
        lifecycle_admission: AdmissionGate,
        retention: OperationRetentionPolicy,
    ) -> Self {
        let mut operation_epoch = [0_u8; OPERATION_EPOCH_BYTES];
        operation_epoch.copy_from_slice(&daemon_instance_id.as_bytes()[..OPERATION_EPOCH_BYTES]);
        let project_paths = coordinator.project_path_space().clone();
        Self {
            admission_gate: Arc::new(Mutex::new(())),
            state: Arc::new(Mutex::new(RegistryState::default())),
            operation_epoch,
            retention,
            tasks: Arc::new(StdMutex::new(OperationTaskState {
                accepting: true,
                tasks: JoinSet::new(),
            })),
            coordinator,
            project_paths,
            query_policy_id,
            lifecycle_admission,
        }
    }

    #[must_use]
    pub const fn query_policy_id(&self) -> QueryPolicyId {
        self.query_policy_id
    }

    async fn coordinator_snapshot(&self) -> crate::coordinator::ReindexCoordinatorSnapshot {
        self.coordinator.snapshot().await
    }

    pub async fn admit_client(
        &self,
        intent: FilesystemReindexIntent,
        idempotency_key: Option<String>,
    ) -> Result<ReindexOperationStatus, ApiError> {
        let internal =
            lower_reindex_intent(&self.project_paths, intent).map_err(project_path_error)?;
        self.admit_internal_with_key(ReindexSource::Ipc, internal, idempotency_key)
            .await
    }

    pub async fn admit_internal(
        &self,
        source: ReindexSource,
        intent: IndexReindexIntent,
    ) -> Result<ReindexOperationStatus, ApiError> {
        self.admit_internal_with_key(source, intent, None).await
    }

    pub async fn admit_watcher_overflow(&self) -> Result<(), ApiError> {
        let query_policy = self.query_policy_id;
        let _lifecycle_admission = self.lifecycle_admission.admit().await.ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotReady,
                "operation registry is draining",
                false,
            )
            .with_detail("lifecycle", "draining")
            .with_query_policy(query_policy)
        })?;
        self.coordinator
            .admit_watcher_overflow_unobserved()
            .await
            .map_err(|error| coordinator_error(error, query_policy))?;
        Ok(())
    }

    #[cfg(test)]
    async fn admit(
        &self,
        _coordinator: &ReindexCoordinator,
        _project_root: &std::path::Path,
        intent: FilesystemReindexIntent,
        idempotency_key: Option<String>,
        query_policy_id: QueryPolicyId,
    ) -> Result<ReindexOperationStatus, ApiError> {
        assert_eq!(query_policy_id, self.query_policy_id);
        self.admit_client(intent, idempotency_key).await
    }

    async fn admit_internal_with_key(
        &self,
        source: ReindexSource,
        internal: IndexReindexIntent,
        idempotency_key: Option<String>,
    ) -> Result<ReindexOperationStatus, ApiError> {
        let query_policy = self.query_policy_id;
        let _lifecycle_admission = self.lifecycle_admission.admit().await.ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::NotReady,
                "operation registry is draining",
                false,
            )
            .with_detail("lifecycle", "draining")
            .with_query_policy(query_policy)
        })?;
        let _admission = self.admission_gate.lock().await;
        {
            let mut tasks = self.tasks.lock().map_err(|_| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    "operation task owner state is poisoned",
                    false,
                )
                .with_query_policy(query_policy)
            })?;
            if !tasks.accepting {
                return Err(ApiError::new(
                    ApiErrorCode::NotReady,
                    "operation registry is draining",
                    false,
                )
                .with_detail("lifecycle", "draining")
                .with_query_policy(query_policy));
            }
            while tasks.tasks.try_join_next().is_some() {}
        }
        let prepared = self
            .coordinator
            .prepare_intent(&internal)
            .map_err(|error| coordinator_error(error, query_policy))?;
        let intent_fingerprint = prepared.fingerprint();
        let existing = {
            let mut state = self.state.lock().await;
            state.prune(Instant::now(), self.retention);
            let existing = idempotency_key
                .as_ref()
                .and_then(|key| state.idempotency.get(key).copied());
            if existing.is_none() && state.active_count >= self.retention.maximum_active {
                return Err(ApiError::new(
                    ApiErrorCode::Busy,
                    "reindex operation registry active limit reached",
                    true,
                )
                .with_detail("maximum", self.retention.maximum_active.to_string())
                .with_query_policy(query_policy));
            }
            existing
        };
        if let Some(existing) = existing {
            if existing.intent_fingerprint != intent_fingerprint {
                return Err(ApiError::new(
                    ApiErrorCode::IdempotencyConflict,
                    "idempotency key is already bound to a different reindex intent",
                    false,
                )
                .with_detail("operation_id", existing.operation_id.to_string())
                .with_query_policy(query_policy));
            }
            return self.status(existing.operation_id, query_policy).await;
        }

        let operation_id = self.unique_operation_id().await;
        let observation = self
            .coordinator
            .admit_prepared_observed(source, prepared)
            .await
            .map_err(|error| coordinator_error(error, query_policy))?;
        let cancellation = observation.cancellation();
        let state = match observation.admission().disposition {
            ReindexDisposition::Coalesced => ReindexOperationState::Coalesced,
            ReindexDisposition::Queued => ReindexOperationState::Queued,
            ReindexDisposition::Applied | ReindexDisposition::AlreadyApplied => {
                ReindexOperationState::Running
            }
        };
        let status = ReindexOperationStatus {
            operation_id,
            state,
            admission: Some(observation.admission().clone()),
            completion: None,
            status: None,
            error: None,
        };
        let entry = Arc::new(OperationEntry {
            status: RwLock::new(status.clone()),
            changed: Notify::new(),
            cancellation,
        });
        {
            let mut registry = self.state.lock().await;
            registry
                .entries
                .insert(operation_id, OperationRecord::Active(Arc::clone(&entry)));
            registry.active_count = registry.active_count.saturating_add(1);
            if let Some(key) = idempotency_key {
                registry.idempotency.insert(
                    key,
                    IdempotencyBinding {
                        operation_id,
                        intent_fingerprint,
                    },
                );
            }
        }
        self.tasks
            .lock()
            .expect("operation task owner was checked while admission remained locked")
            .tasks
            .spawn(complete_operation(
                entry,
                observation,
                operation_id,
                query_policy,
                Arc::clone(&self.state),
                self.retention,
            ));
        Ok(status)
    }

    async fn status(
        &self,
        operation_id: OperationId,
        query_policy: QueryPolicyId,
    ) -> Result<ReindexOperationStatus, ApiError> {
        debug_assert_eq!(query_policy, self.query_policy_id);
        match self.lookup(operation_id).await {
            Some(OperationLookup::Entry(entry)) => Ok(entry.status.read().await.clone()),
            Some(OperationLookup::Snapshot(status)) => Ok(*status),
            None => match self.unknown_operation_error(operation_id, query_policy) {
                Some(error) => Err(error),
                None => Ok(terminal_marker(operation_id, ReindexOperationState::Lost)),
            },
        }
    }

    async fn wait(
        &self,
        operation_id: OperationId,
        timeout: Duration,
        query_policy: QueryPolicyId,
    ) -> Result<ReindexOperationStatus, ApiError> {
        debug_assert_eq!(query_policy, self.query_policy_id);
        let entry = match self.lookup(operation_id).await {
            Some(OperationLookup::Entry(entry)) => entry,
            Some(OperationLookup::Snapshot(status)) => return Ok(*status),
            None => {
                return match self.unknown_operation_error(operation_id, query_policy) {
                    Some(error) => Err(error),
                    None => Ok(terminal_marker(operation_id, ReindexOperationState::Lost)),
                };
            }
        };
        match tokio::time::timeout(timeout, wait_for_terminal(&entry)).await {
            Ok(status) => Ok(status),
            Err(_) => Ok(entry.status.read().await.clone()),
        }
    }

    async fn cancel(
        &self,
        operation_id: OperationId,
        query_policy: QueryPolicyId,
    ) -> Result<ReindexCancelResponse, ApiError> {
        debug_assert_eq!(query_policy, self.query_policy_id);
        let entry = match self.lookup(operation_id).await {
            Some(OperationLookup::Entry(entry)) => entry,
            Some(OperationLookup::Snapshot(status)) => {
                return Ok(ReindexCancelResponse {
                    operation_id,
                    state: status.state,
                    cancelled: false,
                });
            }
            None => {
                let status = match self.unknown_operation_error(operation_id, query_policy) {
                    Some(error) => return Err(error),
                    None => terminal_marker(operation_id, ReindexOperationState::Lost),
                };
                return Ok(ReindexCancelResponse {
                    operation_id,
                    state: status.state,
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
                status.error = None;
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
            let mut registry = self.state.lock().await;
            registry.mark_terminal(operation_id, Instant::now(), self.retention);
            drop(registry);
        }
        if outcome != ReindexCancellationOutcome::Finished {
            entry.changed.notify_waiters();
        }
        let status = if await_terminal {
            wait_for_terminal(&entry).await
        } else {
            entry.status.read().await.clone()
        };
        Ok(ReindexCancelResponse {
            operation_id,
            state: status.state,
            cancelled: status.state == ReindexOperationState::Cancelled,
        })
    }

    async fn unique_operation_id(&self) -> OperationId {
        loop {
            let mut bytes = rand::random::<[u8; 16]>();
            bytes[..OPERATION_EPOCH_BYTES].copy_from_slice(&self.operation_epoch);
            bytes[OPERATION_EPOCH_BYTES] |= 1;
            let candidate = OperationId::from_bytes(bytes);
            if !self.state.lock().await.entries.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    async fn lookup(&self, operation_id: OperationId) -> Option<OperationLookup> {
        let mut state = self.state.lock().await;
        state.prune(Instant::now(), self.retention);
        state.entries.get(&operation_id).map(|record| match record {
            OperationRecord::Active(entry) | OperationRecord::Terminal(entry) => {
                OperationLookup::Entry(Arc::clone(entry))
            }
            OperationRecord::Expired(status) => OperationLookup::Snapshot(status.clone()),
        })
    }

    fn unknown_operation_error(
        &self,
        operation_id: OperationId,
        query_policy: QueryPolicyId,
    ) -> Option<ApiError> {
        if operation_id.as_bytes()[..OPERATION_EPOCH_BYTES] != self.operation_epoch {
            return None;
        }
        Some(operation_not_found(query_policy))
    }
}

impl OperationRegistryOwner {
    pub fn new(
        daemon_instance_id: DaemonInstanceId,
        coordinator: ReindexCoordinator,
        query_policy_id: QueryPolicyId,
        lifecycle_admission: AdmissionGate,
    ) -> Self {
        Self {
            registry: OperationRegistry::new(
                daemon_instance_id,
                coordinator,
                query_policy_id,
                lifecycle_admission,
            ),
            draining: None,
        }
    }

    #[must_use]
    pub fn registry(&self) -> OperationRegistry {
        self.registry.clone()
    }

    /// Closes operation admission and joins all connection-independent completion observers.
    pub async fn shutdown(&mut self) -> Result<(), anyhow::Error> {
        self.registry.lifecycle_admission.begin_draining().await;
        if self.draining.is_none() {
            let _admission = self.registry.admission_gate.lock().await;
            let mut tasks = self
                .registry
                .tasks
                .lock()
                .map_err(|_| anyhow::anyhow!("operation task owner state is poisoned"))?;
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
                first_failure = Some(error.to_string());
            }
        }
        self.draining = None;
        match first_failure {
            Some(error) => Err(anyhow::anyhow!("operation completion task failed: {error}")),
            None => Ok(()),
        }
    }
}

enum OperationLookup {
    Entry(Arc<OperationEntry>),
    Snapshot(Box<ReindexOperationStatus>),
}

async fn wait_for_terminal(entry: &OperationEntry) -> ReindexOperationStatus {
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
    query_policy: QueryPolicyId,
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
                status.error = None;
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
                    Ok(completion) => ReindexOperationStatus {
                        operation_id,
                        state: ReindexOperationState::Succeeded,
                        admission: Some(completion.admission),
                        completion: Some(completion.terminal),
                        status: Some(completion.status),
                        error: None,
                    },
                    Err(error) => ReindexOperationStatus {
                        operation_id,
                        state: ReindexOperationState::Failed,
                        admission: coordinator_admission(&error),
                        completion: None,
                        status: None,
                        error: Some(coordinator_error(error, query_policy)),
                    },
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

fn advance_active_state(status: &mut ReindexOperationStatus, observed: ReindexOperationState) {
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

fn coordinator_admission(
    error: &CoordinatorError,
) -> Option<unity_asset_search_protocol::ReindexReceipt> {
    match error {
        CoordinatorError::ExecutionFailed { admission, .. }
        | CoordinatorError::CompletionChannelClosed { admission } => Some((**admission).clone()),
        _ => None,
    }
}

fn lower_reindex_intent(
    project_paths: &ProjectPathSpace,
    intent: FilesystemReindexIntent,
) -> Result<IndexReindexIntent, ProjectPathError> {
    let scope = match intent.scope {
        FilesystemReindexScope::Full => IndexReindexScope::Full,
        FilesystemReindexScope::Reconcile => IndexReindexScope::Reconcile,
        FilesystemReindexScope::ChangedPaths { paths } => IndexReindexScope::ChangedPaths {
            paths: project_paths
                .resolve_set(paths.iter().map(|path| std::path::Path::new(path.as_str())))?,
        },
    };
    Ok(IndexReindexIntent { scope })
}

fn project_path_error(error: ProjectPathError) -> ApiError {
    let detail = bounded_detail(error.to_string());
    match error {
        ProjectPathError::OutsideProject { .. }
        | ProjectPathError::InvalidComponent { .. }
        | ProjectPathError::ProjectRootChangedPath { .. } => ApiError::new(
            ApiErrorCode::InvalidRequest,
            "reindex path is not a valid project-relative coordinate",
            false,
        )
        .with_detail("cause", detail),
        ProjectPathError::Allocation { .. } => ApiError::new(
            ApiErrorCode::Busy,
            "could not allocate normalized reindex paths",
            true,
        )
        .with_detail("cause", detail),
        _ => ApiError::new(
            ApiErrorCode::Internal,
            "could not normalize reindex paths",
            false,
        )
        .with_detail("cause", detail),
    }
}

fn coordinator_error(error: CoordinatorError, query_policy: QueryPolicyId) -> ApiError {
    let error = match error {
        CoordinatorError::CompletionWaiterLimit { maximum } => ApiError::new(
            ApiErrorCode::Busy,
            "reindex completion waiter limit reached",
            true,
        )
        .with_detail("maximum", maximum.to_string()),
        CoordinatorError::CompletionWaiterAllocationFailed => ApiError::new(
            ApiErrorCode::Busy,
            "could not allocate a reindex completion waiter",
            true,
        ),
        CoordinatorError::ExecutionFailed { scope, message, .. } => ApiError::new(
            ApiErrorCode::IndexBuildFailed,
            "reindex execution failed",
            true,
        )
        .with_detail("scope", format!("{scope:?}"))
        .with_detail("cause", bounded_detail(message)),
        CoordinatorError::CompletionChannelClosed { .. } => ApiError::new(
            ApiErrorCode::Internal,
            "reindex completion channel closed unexpectedly",
            true,
        ),
        CoordinatorError::ChangedPathProjectMismatch { .. } => ApiError::new(
            ApiErrorCode::Internal,
            "reindex path set belongs to a different project",
            false,
        ),
        CoordinatorError::InvalidConfiguration(_) => ApiError::new(
            ApiErrorCode::Internal,
            "reindex coordinator configuration is invalid",
            false,
        ),
        CoordinatorError::ShuttingDown => ApiError::new(
            ApiErrorCode::NotReady,
            "reindex coordinator is draining",
            false,
        )
        .with_detail("lifecycle", "draining"),
        CoordinatorError::RunnerTerminated { message } => ApiError::new(
            ApiErrorCode::Internal,
            "reindex coordinator runner terminated unexpectedly",
            false,
        )
        .with_detail("cause", bounded_detail(message)),
    };
    error.with_query_policy(query_policy)
}

fn bounded_detail(mut value: String) -> String {
    const MAXIMUM: usize = 4 * 1024;
    if value.len() <= MAXIMUM {
        return value;
    }
    let mut boundary = MAXIMUM;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

fn operation_not_found(query_policy: QueryPolicyId) -> ApiError {
    ApiError::new(
        ApiErrorCode::OperationNotFound,
        "reindex operation was not found",
        false,
    )
    .with_query_policy(query_policy)
}

fn terminal_marker(
    operation_id: OperationId,
    state: ReindexOperationState,
) -> ReindexOperationStatus {
    debug_assert!(matches!(
        state,
        ReindexOperationState::Expired | ReindexOperationState::Lost
    ));
    ReindexOperationStatus {
        operation_id,
        state,
        admission: None,
        completion: None,
        status: None,
        error: None,
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
            if !matches!(
                self.entries.get(&operation_id),
                Some(OperationRecord::Terminal(_))
            ) {
                continue;
            }
            self.entries.insert(
                operation_id,
                OperationRecord::Expired(Box::new(terminal_marker(
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
                self.entries.remove(&operation_id);
                self.idempotency
                    .retain(|_, retained| retained.operation_id != operation_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Semaphore;
    use tokio::time::Instant;
    use unity_asset_search_index::{IndexPaths, ProjectPathSpace};
    use unity_asset_search_protocol::{
        ApiErrorCode, DaemonInstanceId, DaemonLifecycleState, DaemonLifecycleStatus,
        FreshnessMaintenance, GenerationFreshness, OperationId, OperationKind, PortablePath,
        QueryPolicyId, ReconcileLifecycle, ReindexOperationState, ReindexOperationStatus,
        RequestOperation, ServingAvailability, ShutdownRequest, TimerLifecycleState, TimerStatus,
        ValidateContract, WatcherLifecycleState, WatcherStatus,
    };

    use super::{
        DispatcherShutdown, OperationRegistry, OperationRetentionPolicy, advance_active_state,
        daemon_lifecycle_status, requested_shutdown_deadline, requires_lifecycle_admission,
        tighten_shutdown,
    };
    use crate::coordinator::{
        ReindexAdmissionCounts, ReindexCoordinator, ReindexCoordinatorConfig,
        ReindexCoordinatorRuntime, ReindexCoordinatorSnapshot, ReindexFailure, ReindexScopeKind,
    };
    use crate::lifecycle::AdmissionGate;
    use crate::watcher::{MaintenanceSnapshot, TimerLifecycle, WatcherLifecycle};

    fn project_root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\unity-asset-operation-registry-tests")
        } else {
            PathBuf::from("/unity-asset-operation-registry-tests")
        }
    }

    fn project_paths() -> ProjectPathSpace {
        let project = crate::secure_test_tempdir();
        let assets = project.path().join("Assets");
        std::fs::create_dir(&assets).expect("create operation registry Assets root");
        let paths = IndexPaths::for_project(
            project.path().to_path_buf(),
            Some(project.path().join(".unity-asset-index")),
            Some(vec![assets]),
        )
        .expect("create operation registry project path space");
        paths.project_path_space().clone()
    }

    fn delayed_config() -> ReindexCoordinatorConfig {
        ReindexCoordinatorConfig::new(project_paths())
            .with_debounce(Duration::from_secs(60))
            .with_max_debounce(Duration::from_secs(60))
    }

    fn registry(instance_byte: u8, coordinator: ReindexCoordinator) -> OperationRegistry {
        OperationRegistry::new(
            DaemonInstanceId::from_bytes([instance_byte; 16]),
            coordinator,
            query_policy(),
            AdmissionGate::default(),
        )
    }

    fn query_policy() -> QueryPolicyId {
        QueryPolicyId::from_bytes([7; 32])
    }

    fn active_status(state: ReindexOperationState) -> ReindexOperationStatus {
        ReindexOperationStatus {
            operation_id: OperationId::from_bytes([1; 16]),
            state,
            admission: None,
            completion: None,
            status: None,
            error: None,
        }
    }

    #[test]
    fn active_state_transitions_are_monotonic() {
        let mut status = active_status(ReindexOperationState::Running);
        advance_active_state(&mut status, ReindexOperationState::Coalesced);
        assert_eq!(status.state, ReindexOperationState::Running);

        let mut status = active_status(ReindexOperationState::Queued);
        advance_active_state(&mut status, ReindexOperationState::Coalesced);
        assert_eq!(status.state, ReindexOperationState::Coalesced);
        advance_active_state(&mut status, ReindexOperationState::Running);
        assert_eq!(status.state, ReindexOperationState::Running);
    }

    #[test]
    fn daemon_status_reports_current_lifecycle_not_historical_failures() {
        let index_status = DaemonLifecycleStatus {
            lifecycle: DaemonLifecycleState::Serving,
            serving: ServingAvailability::Queryable,
            freshness: GenerationFreshness::Current,
            freshness_maintenance: FreshnessMaintenance::Unmanaged,
            reconcile: ReconcileLifecycle::Idle,
            generation_maintenance: unity_asset_search_protocol::GenerationMaintenanceStatus::clean(
            ),
            watcher: WatcherStatus {
                state: WatcherLifecycleState::Disabled,
                retry_count: 0,
                last_failure: None,
                next_retry_in_ms: None,
            },
            timer: TimerStatus {
                state: TimerLifecycleState::Disabled,
                run_count: 0,
                last_failure: None,
                next_run_in_ms: None,
            },
        };
        let coordinator = ReindexCoordinatorSnapshot {
            running: false,
            in_flight: None,
            pending_general: None,
            last_completion_failed: false,
            failures: vec![ReindexFailure {
                sequence: 1,
                scope: ReindexScopeKind::Full,
                message: "historical failure".to_owned(),
            }],
            full_escalations: 0,
            watcher_overflows: 0,
            admissions: ReindexAdmissionCounts::default(),
        };
        let maintenance = MaintenanceSnapshot {
            watcher: WatcherLifecycle::Disabled,
            watcher_retry_count: 0,
            watcher_last_failure: None,
            watcher_next_retry_in_ms: None,
            timer: TimerLifecycle::Disabled,
            timer_run_count: 0,
            timer_last_failure: None,
            timer_next_run_in_ms: None,
        };

        let status =
            daemon_lifecycle_status(false, &index_status, &coordinator, maintenance.clone());
        assert_eq!(status.lifecycle, DaemonLifecycleState::Serving);
        assert_eq!(status.reconcile, ReconcileLifecycle::Idle);
        assert_eq!(
            status.freshness_maintenance,
            FreshnessMaintenance::Unmanaged
        );

        let status = daemon_lifecycle_status(
            true,
            &index_status,
            &ReindexCoordinatorSnapshot {
                pending_general: Some(ReindexScopeKind::Reconcile),
                ..coordinator
            },
            maintenance,
        );
        assert_eq!(status.lifecycle, DaemonLifecycleState::Draining);
        assert_eq!(status.reconcile, ReconcileLifecycle::Queued);
    }

    #[tokio::test]
    async fn draining_gate_rejects_new_work_but_preserves_bounded_observation() {
        let gate = AdmissionGate::default();
        assert!(gate.admit().await.is_some());
        gate.begin_draining().await;

        for rejected in [
            OperationKind::Search,
            OperationKind::Suggest,
            OperationKind::References,
            OperationKind::ReindexAdmit,
            OperationKind::ReindexWait,
            OperationKind::ReindexCancel,
        ] {
            if requires_lifecycle_admission(rejected) {
                assert!(
                    gate.admit().await.is_none(),
                    "{rejected:?} must be rejected"
                );
            } else {
                assert_eq!(rejected, OperationKind::ReindexAdmit);
                assert!(gate.admit().await.is_none());
            }
        }
        for allowed in [
            OperationKind::Capabilities,
            OperationKind::Status,
            OperationKind::ReindexStatus,
            OperationKind::Shutdown,
        ] {
            assert!(
                !requires_lifecycle_admission(allowed),
                "{allowed:?} must remain observable"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_deadline_is_absolute_and_can_only_tighten() {
        let now = Instant::now();
        let requested = requested_shutdown_deadline(&RequestOperation::Shutdown(ShutdownRequest {
            drain_timeout_ms: 5_000,
        }));
        assert_eq!(requested, Some(now + Duration::from_secs(5)));

        let mut current = None;
        assert!(tighten_shutdown(&mut current, now + Duration::from_secs(5)));
        assert!(!tighten_shutdown(
            &mut current,
            now + Duration::from_secs(10)
        ));
        assert_eq!(current, Some(now + Duration::from_secs(5)));
        assert!(tighten_shutdown(&mut current, now));
        assert_eq!(current, Some(now));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_signal_closes_admission_before_publishing_earliest_deadline() {
        let admission = AdmissionGate::default();
        let _existing = admission.admit().await.unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(None);
        let handle = DispatcherShutdown {
            shutdown,
            admission: admission.clone(),
        };
        let now = Instant::now();

        handle.begin_shutdown_at(now + Duration::from_secs(5));

        assert!(admission.admit().await.is_none());
        assert_eq!(*receiver.borrow(), Some(now + Duration::from_secs(5)));

        handle.begin_shutdown_at(now + Duration::from_secs(10));
        assert_eq!(*receiver.borrow(), Some(now + Duration::from_secs(5)));
        handle.begin_shutdown_at(now + Duration::from_secs(1));
        assert_eq!(*receiver.borrow(), Some(now + Duration::from_secs(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn queued_exclusive_operation_can_be_cancelled_without_starting_executor() {
        let builds = Arc::new(AtomicUsize::new(0));
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), {
            let builds = Arc::clone(&builds);
            move |_intent| {
                let builds = Arc::clone(&builds);
                async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    std::future::pending().await
                }
            }
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = registry(1, coordinator.clone());
        let admitted = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                None,
                query_policy(),
            )
            .await
            .unwrap();

        assert_eq!(
            admitted.state,
            unity_asset_search_protocol::ReindexOperationState::Queued
        );
        let cancelled = registry
            .cancel(admitted.operation_id, query_policy())
            .await
            .unwrap();
        assert!(cancelled.cancelled);
        assert_eq!(
            cancelled.state,
            unity_asset_search_protocol::ReindexOperationState::Cancelled
        );
        let repeated = registry
            .cancel(admitted.operation_id, query_policy())
            .await
            .unwrap();
        assert!(repeated.cancelled);
        repeated.validate().unwrap();
        coordinator.wait_for_idle().await;
        assert_eq!(builds.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn equivalent_normalized_intents_reuse_one_idempotent_operation() {
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), |_intent| async move {
            std::future::pending().await
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = registry(10, coordinator.clone());
        let path_a = PortablePath::new("Assets/A.prefab").unwrap();
        let path_b = PortablePath::new("Assets/B.prefab").unwrap();
        let first = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::changed_paths(vec![
                    path_b.clone(),
                    path_a.clone(),
                    path_a.clone(),
                ]),
                Some("normalized-intent".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();
        let repeated = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::changed_paths(vec![
                    path_a, path_b,
                ]),
                Some("normalized-intent".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();

        assert_eq!(repeated.operation_id, first.operation_id);
        assert_eq!(coordinator.snapshot().await.admissions.ipc, 1);
    }

    #[cfg(windows)]
    #[tokio::test(start_paused = true)]
    async fn windows_case_aliases_share_one_idempotent_operation() {
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), |_intent| async move {
            std::future::pending().await
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = registry(13, coordinator.clone());
        let first = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::changed_paths(vec![
                    PortablePath::new("Assets/Hero.prefab").unwrap(),
                ]),
                Some("windows-path-alias".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();
        let repeated = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::changed_paths(vec![
                    PortablePath::new("assets/HERO.prefab").unwrap(),
                ]),
                Some("windows-path-alias".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();

        assert_eq!(repeated.operation_id, first.operation_id);
        assert_eq!(coordinator.snapshot().await.admissions.ipc, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn idempotency_key_rejects_different_normalized_intents() {
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), |_intent| async move {
            std::future::pending().await
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = registry(11, coordinator.clone());
        let admitted = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                Some("conflicting-intent".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();

        for conflicting in [
            unity_asset_search_protocol::FilesystemReindexIntent::reconcile(),
            unity_asset_search_protocol::FilesystemReindexIntent::changed_paths(vec![
                PortablePath::new("Assets/Different.prefab").unwrap(),
            ]),
        ] {
            let error = registry
                .admit(
                    &coordinator,
                    &project_root(),
                    conflicting,
                    Some("conflicting-intent".to_owned()),
                    query_policy(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, ApiErrorCode::IdempotencyConflict);
            assert!(!error.retryable);
            assert_eq!(
                error.details.get("operation_id"),
                Some(&admitted.operation_id.to_string())
            );
        }
        assert_eq!(coordinator.snapshot().await.admissions.ipc, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn shared_draining_gate_rejects_ipc_watcher_and_timer_before_counting() {
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), |_intent| async move {
            std::future::pending().await
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let lifecycle_admission = AdmissionGate::default();
        let registry = OperationRegistry::new(
            DaemonInstanceId::from_bytes([12; 16]),
            coordinator.clone(),
            query_policy(),
            lifecycle_admission.clone(),
        );
        registry
            .admit_internal(
                crate::coordinator::ReindexSource::Startup,
                unity_asset_search_index::FilesystemReindexIntent::reconcile(),
            )
            .await
            .unwrap();
        let (shutdown, _) = tokio::sync::watch::channel(None);
        DispatcherShutdown {
            shutdown,
            admission: lifecycle_admission.clone(),
        }
        .begin_shutdown_at(Instant::now());

        let ipc_error = registry
            .admit_client(
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                None,
            )
            .await
            .unwrap_err();
        let watcher_error = registry
            .admit_internal(
                crate::coordinator::ReindexSource::Watcher,
                unity_asset_search_index::FilesystemReindexIntent::full(),
            )
            .await
            .unwrap_err();
        let watcher_overflow_error = registry.admit_watcher_overflow().await.unwrap_err();
        let timer_error = registry
            .admit_internal(
                crate::coordinator::ReindexSource::Timer,
                unity_asset_search_index::FilesystemReindexIntent::reconcile(),
            )
            .await
            .unwrap_err();
        let startup_error = registry
            .admit_internal(
                crate::coordinator::ReindexSource::Startup,
                unity_asset_search_index::FilesystemReindexIntent::reconcile(),
            )
            .await
            .unwrap_err();

        for error in [
            ipc_error,
            watcher_error,
            watcher_overflow_error,
            timer_error,
            startup_error,
        ] {
            assert_eq!(error.code, ApiErrorCode::NotReady);
            assert_eq!(
                error.details.get("lifecycle").map(String::as_str),
                Some("draining")
            );
        }
        let admissions = coordinator.snapshot().await.admissions;
        assert_eq!(admissions.startup, 1);
        assert_eq!(admissions.ipc, 0);
        assert_eq!(admissions.watcher, 0);
        assert_eq!(admissions.timer, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_overflow_bypasses_operation_status_saturation() {
        let retention = OperationRetentionPolicy {
            maximum_active: 1,
            maximum_terminal: 4,
            maximum_expired: 4,
            terminal_retention: Duration::from_secs(60),
            expired_retention: Duration::from_secs(60),
        };
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), |_intent| async move {
            std::future::pending().await
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = OperationRegistry::with_retention(
            DaemonInstanceId::from_bytes([13; 16]),
            coordinator.clone(),
            query_policy(),
            AdmissionGate::default(),
            retention,
        );
        registry
            .admit_internal(
                crate::coordinator::ReindexSource::Startup,
                unity_asset_search_index::FilesystemReindexIntent::reconcile(),
            )
            .await
            .unwrap();

        let busy = registry
            .admit_client(
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(busy.code, ApiErrorCode::Busy);

        registry.admit_watcher_overflow().await.unwrap();

        let snapshot = coordinator.snapshot().await;
        assert_eq!(snapshot.watcher_overflows, 1);
        assert_eq!(snapshot.full_escalations, 1);
        assert_eq!(snapshot.admissions.watcher, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn coalesced_operations_reject_cancellation_and_share_one_execution() {
        let builds = Arc::new(AtomicUsize::new(0));
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), {
            let builds = Arc::clone(&builds);
            move |_intent| {
                let builds = Arc::clone(&builds);
                async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    anyhow::bail!("injected terminal failure")
                }
            }
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = registry(2, coordinator.clone());
        let first = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                None,
                query_policy(),
            )
            .await
            .unwrap();
        let second = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::reconcile(),
                None,
                query_policy(),
            )
            .await
            .unwrap();
        let third = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                None,
                query_policy(),
            )
            .await
            .unwrap();

        let first_cancel = registry
            .cancel(first.operation_id, query_policy())
            .await
            .unwrap();
        assert!(!first_cancel.cancelled);
        assert_eq!(
            first_cancel.state,
            unity_asset_search_protocol::ReindexOperationState::Coalesced
        );
        let second_cancel = registry
            .cancel(second.operation_id, query_policy())
            .await
            .unwrap();
        assert!(!second_cancel.cancelled);
        assert_eq!(
            second_cancel.state,
            unity_asset_search_protocol::ReindexOperationState::Coalesced
        );
        let third_cancel = registry
            .cancel(third.operation_id, query_policy())
            .await
            .unwrap();
        assert!(!third_cancel.cancelled);
        assert_eq!(
            third_cancel.state,
            unity_asset_search_protocol::ReindexOperationState::Coalesced
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        coordinator.wait_for_idle().await;
        tokio::task::yield_now().await;

        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry
                .status(first.operation_id, query_policy())
                .await
                .unwrap()
                .state,
            unity_asset_search_protocol::ReindexOperationState::Failed
        );
        assert_eq!(
            registry
                .status(second.operation_id, query_policy())
                .await
                .unwrap()
                .state,
            unity_asset_search_protocol::ReindexOperationState::Failed
        );
        assert_eq!(
            registry
                .status(third.operation_id, query_policy())
                .await
                .unwrap()
                .state,
            unity_asset_search_protocol::ReindexOperationState::Failed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_event_count_never_restores_exclusive_cancellation() {
        let builds = Arc::new(AtomicUsize::new(0));
        let _runtime =
            ReindexCoordinatorRuntime::start(delayed_config().with_max_pending_events(1), {
                let builds = Arc::clone(&builds);
                move |_intent| {
                    let builds = Arc::clone(&builds);
                    async move {
                        builds.fetch_add(1, Ordering::SeqCst);
                        anyhow::bail!("injected terminal failure")
                    }
                }
            })
            .unwrap();
        let coordinator = _runtime.coordinator();
        coordinator
            .admit(
                crate::coordinator::ReindexSource::Watcher,
                unity_asset_search_index::FilesystemReindexIntent::full(),
            )
            .await
            .unwrap();
        let registry = registry(6, coordinator.clone());
        let observed = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::reconcile(),
                None,
                query_policy(),
            )
            .await
            .unwrap();

        let cancellation = registry
            .cancel(observed.operation_id, query_policy())
            .await
            .unwrap();
        assert!(!cancellation.cancelled);
        assert_eq!(
            cancellation.state,
            unity_asset_search_protocol::ReindexOperationState::Coalesced
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        coordinator.wait_for_idle().await;
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn running_operation_rejects_cancellation() {
        let started = Arc::new(Semaphore::new(0));
        let finish = Arc::new(Semaphore::new(0));
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), {
            let started = Arc::clone(&started);
            let finish = Arc::clone(&finish);
            move |_intent| {
                let started = Arc::clone(&started);
                let finish = Arc::clone(&finish);
                async move {
                    started.add_permits(1);
                    let permit = finish.acquire().await.unwrap();
                    permit.forget();
                    anyhow::bail!("injected terminal failure")
                }
            }
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = registry(3, coordinator.clone());
        let admitted = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                None,
                query_policy(),
            )
            .await
            .unwrap();

        tokio::time::advance(Duration::from_secs(60)).await;
        let permit = started.acquire().await.unwrap();
        permit.forget();
        tokio::task::yield_now().await;
        let cancellation = registry
            .cancel(admitted.operation_id, query_policy())
            .await
            .unwrap();
        assert!(!cancellation.cancelled);
        assert_eq!(
            cancellation.state,
            unity_asset_search_protocol::ReindexOperationState::Running
        );

        finish.add_permits(1);
        coordinator.wait_for_idle().await;
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_records_expire_by_time_and_prior_daemon_ids_are_lost() {
        let retention = OperationRetentionPolicy {
            maximum_active: 4,
            maximum_terminal: 4,
            maximum_expired: 4,
            terminal_retention: Duration::from_secs(1),
            expired_retention: Duration::from_secs(1),
        };
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), |_intent| async move {
            std::future::pending().await
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = OperationRegistry::with_retention(
            DaemonInstanceId::from_bytes([4; 16]),
            coordinator.clone(),
            query_policy(),
            AdmissionGate::default(),
            retention,
        );
        let admitted = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                Some("retry-key".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();
        assert!(
            registry
                .cancel(admitted.operation_id, query_policy())
                .await
                .unwrap()
                .cancelled
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            registry
                .status(admitted.operation_id, query_policy())
                .await
                .unwrap()
                .state,
            unity_asset_search_protocol::ReindexOperationState::Expired
        );
        let retried = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                Some("retry-key".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();
        assert_eq!(retried.operation_id, admitted.operation_id);
        assert_eq!(
            retried.state,
            unity_asset_search_protocol::ReindexOperationState::Expired
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            registry
                .status(admitted.operation_id, query_policy())
                .await
                .unwrap_err()
                .code,
            ApiErrorCode::OperationNotFound
        );

        let prior_id = unity_asset_search_protocol::OperationId::from_bytes([9; 16]);
        assert_eq!(
            registry
                .status(prior_id, query_policy())
                .await
                .unwrap()
                .state,
            unity_asset_search_protocol::ReindexOperationState::Lost
        );
    }

    #[tokio::test(start_paused = true)]
    async fn one_idle_interval_cannot_extend_terminal_and_idempotency_retention() {
        let retention = OperationRetentionPolicy {
            maximum_active: 4,
            maximum_terminal: 4,
            maximum_expired: 4,
            terminal_retention: Duration::from_secs(1),
            expired_retention: Duration::from_secs(1),
        };
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), |_intent| async move {
            std::future::pending().await
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = OperationRegistry::with_retention(
            DaemonInstanceId::from_bytes([8; 16]),
            coordinator.clone(),
            query_policy(),
            AdmissionGate::default(),
            retention,
        );
        let admitted = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::full(),
                Some("idle-window".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();
        assert!(
            registry
                .cancel(admitted.operation_id, query_policy())
                .await
                .unwrap()
                .cancelled
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            registry
                .status(admitted.operation_id, query_policy())
                .await
                .unwrap_err()
                .code,
            ApiErrorCode::OperationNotFound
        );
        let retried = registry
            .admit(
                &coordinator,
                &project_root(),
                unity_asset_search_protocol::FilesystemReindexIntent::reconcile(),
                Some("idle-window".to_owned()),
                query_policy(),
            )
            .await
            .unwrap();
        assert_ne!(retried.operation_id, admitted.operation_id);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_and_expired_records_are_each_count_bounded() {
        let retention = OperationRetentionPolicy {
            maximum_active: 4,
            maximum_terminal: 1,
            maximum_expired: 1,
            terminal_retention: Duration::from_secs(60),
            expired_retention: Duration::from_secs(60),
        };
        let _runtime = ReindexCoordinatorRuntime::start(delayed_config(), |_intent| async move {
            std::future::pending().await
        })
        .unwrap();
        let coordinator = _runtime.coordinator();
        let registry = OperationRegistry::with_retention(
            DaemonInstanceId::from_bytes([5; 16]),
            coordinator.clone(),
            query_policy(),
            AdmissionGate::default(),
            retention,
        );
        let mut operation_ids = Vec::new();

        for _ in 0..3 {
            let admitted = registry
                .admit(
                    &coordinator,
                    &project_root(),
                    unity_asset_search_protocol::FilesystemReindexIntent::full(),
                    None,
                    query_policy(),
                )
                .await
                .unwrap();
            assert!(
                registry
                    .cancel(admitted.operation_id, query_policy())
                    .await
                    .unwrap()
                    .cancelled
            );
            operation_ids.push(admitted.operation_id);
            coordinator.wait_for_idle().await;
        }

        assert_eq!(
            registry
                .status(operation_ids[1], query_policy())
                .await
                .unwrap()
                .state,
            unity_asset_search_protocol::ReindexOperationState::Expired
        );
        assert_eq!(
            registry
                .status(operation_ids[2], query_policy())
                .await
                .unwrap()
                .state,
            unity_asset_search_protocol::ReindexOperationState::Cancelled
        );
        assert_eq!(
            registry
                .status(operation_ids[0], query_policy())
                .await
                .unwrap_err()
                .code,
            ApiErrorCode::OperationNotFound
        );
    }
}
