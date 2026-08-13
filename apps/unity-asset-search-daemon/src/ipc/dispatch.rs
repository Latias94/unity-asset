use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};
use unity_asset_search_index::{
    FilesystemReindexIntent as IndexReindexIntent, FilesystemReindexScope as IndexReindexScope,
    ProjectPathError, ProjectPathSpace, SearchIndex, SearchRequest as IndexSearchRequest,
};
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, BackgroundReindexOperation, CapabilitiesResponse, DaemonLifecycleState,
    DaemonLifecycleStatus, FilesystemReindexIntent, FilesystemReindexScope, FreshnessMaintenance,
    QueryPolicyId, ReconcileLifecycle, ReindexCancelResponse, ReindexOperationStatus,
    RequestOperation, ResponseOperation, SearchCapabilities, ShutdownResponse, TimerLifecycleState,
    TimerStatus, WatcherLifecycleState, WatcherStatus,
};

use crate::coordinator::{CoordinatorError, ReindexScopeKind};
use crate::lifecycle::{AdmissionGate, BlockingTaskError, BlockingTaskHandle};
use crate::operations::{
    OperationCancellation, OperationError, OperationFailure, OperationOrigin, OperationService,
    OperationSnapshot,
};
use crate::watcher::{
    MaintenanceHandle, MaintenanceSnapshot, TimerLifecycle as InternalTimerLifecycle,
    WatcherLifecycle as InternalWatcherLifecycle,
};

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
    operations: OperationService,
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
        operations: OperationService,
        query_policy_id: QueryPolicyId,
        admission: AdmissionGate,
        maintenance: MaintenanceHandle,
    ) -> Self {
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
                    daemon_version: crate::build_identity::VERSION_REPORT.to_owned(),
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
            RequestOperation::ReindexAdmit(request) => {
                let internal = lower_reindex_intent(
                    self.inner.operations.project_path_space(),
                    request.intent,
                )
                .map_err(project_path_error);
                match internal {
                    Ok(intent) => self
                        .inner
                        .operations
                        .admit(OperationOrigin::Ipc, intent, request.idempotency_key)
                        .await
                        .map(|status| {
                            ResponseOperation::ReindexAdmit(operation_status(
                                status,
                                self.inner.query_policy_id,
                            ))
                        })
                        .map_err(|error| operation_error(error, self.inner.query_policy_id)),
                    Err(error) => Err(error.with_query_policy(self.inner.query_policy_id)),
                }
            }
            RequestOperation::ReindexStatus(request) => self
                .inner
                .operations
                .status(request.operation_id)
                .await
                .map(|status| {
                    ResponseOperation::ReindexStatus(operation_status(
                        status,
                        self.inner.query_policy_id,
                    ))
                })
                .map_err(|error| operation_error(error, self.inner.query_policy_id)),
            RequestOperation::ReindexWait(request) => self
                .inner
                .operations
                .wait(
                    request.operation_id,
                    Duration::from_millis(u64::from(request.timeout_ms)),
                )
                .await
                .map(|status| {
                    ResponseOperation::ReindexWait(operation_status(
                        status,
                        self.inner.query_policy_id,
                    ))
                })
                .map_err(|error| operation_error(error, self.inner.query_policy_id)),
            RequestOperation::ReindexCancel(request) => self
                .inner
                .operations
                .cancel(request.operation_id)
                .await
                .map(|cancellation| {
                    ResponseOperation::ReindexCancel(operation_cancellation(cancellation))
                })
                .map_err(|error| operation_error(error, self.inner.query_policy_id)),
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
        let background_reindex_operations = self
            .inner
            .operations
            .background_operations()
            .await
            .map_err(|error| operation_error(error, self.inner.query_policy_id))?;
        status.daemon = daemon_lifecycle_status(
            self.inner.admission.is_draining().await,
            &status.daemon,
            &coordinator,
            maintenance,
            background_reindex_operations,
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
    background_reindex_operations: Vec<BackgroundReindexOperation>,
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
        background_reindex_operations,
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

fn operation_error(error: OperationError, query_policy: QueryPolicyId) -> ApiError {
    match error {
        OperationError::Draining => ApiError::new(
            ApiErrorCode::NotReady,
            "operation service is draining",
            false,
        )
        .with_detail("lifecycle", "draining")
        .with_query_policy(query_policy),
        OperationError::Saturated { maximum } => ApiError::new(
            ApiErrorCode::Busy,
            "reindex operation service active limit reached",
            true,
        )
        .with_detail("maximum", maximum.to_string())
        .with_query_policy(query_policy),
        OperationError::IdempotencyConflict { operation_id } => ApiError::new(
            ApiErrorCode::IdempotencyConflict,
            "idempotency key is already bound to a different reindex intent",
            false,
        )
        .with_detail("operation_id", operation_id.to_string())
        .with_query_policy(query_policy),
        OperationError::CompletionTaskTerminated { message } => ApiError::new(
            ApiErrorCode::Internal,
            "operation completion observer terminated unexpectedly",
            false,
        )
        .with_detail("cause", bounded_detail(message))
        .with_query_policy(query_policy),
        OperationError::NotFound => ApiError::new(
            ApiErrorCode::OperationNotFound,
            "reindex operation was not found",
            false,
        )
        .with_query_policy(query_policy),
        OperationError::ControlForbidden { origin } => ApiError::new(
            ApiErrorCode::OperationControlForbidden,
            "daemon-owned reindex operation cannot be cancelled by a client",
            false,
        )
        .with_detail("origin", origin.wire_name())
        .with_query_policy(query_policy),
        OperationError::RegistryInvariant { message } => ApiError::new(
            ApiErrorCode::Internal,
            "operation registry invariant failed",
            false,
        )
        .with_detail("cause", message)
        .with_query_policy(query_policy),
        OperationError::Coordinator(error) => coordinator_error(error, query_policy),
    }
}

fn operation_status(
    snapshot: OperationSnapshot,
    query_policy: QueryPolicyId,
) -> ReindexOperationStatus {
    ReindexOperationStatus {
        operation_id: snapshot.operation_id,
        state: snapshot.state,
        admission: snapshot.admission,
        completion: snapshot.completion,
        status: snapshot.status,
        error: snapshot
            .failure
            .map(|failure| operation_failure(failure, query_policy)),
    }
}

fn operation_failure(failure: OperationFailure, query_policy: QueryPolicyId) -> ApiError {
    match failure {
        OperationFailure::Execution { scope, message } => ApiError::new(
            ApiErrorCode::IndexBuildFailed,
            "reindex execution failed",
            true,
        )
        .with_detail("scope", reindex_scope_name(scope))
        .with_detail("cause", bounded_detail(message))
        .with_query_policy(query_policy),
        OperationFailure::CompletionChannelClosed => ApiError::new(
            ApiErrorCode::Internal,
            "reindex completion channel closed unexpectedly",
            true,
        )
        .with_query_policy(query_policy),
    }
}

const fn reindex_scope_name(scope: ReindexScopeKind) -> &'static str {
    match scope {
        ReindexScopeKind::Full => "full",
        ReindexScopeKind::Reconcile => "reconcile",
        ReindexScopeKind::ChangedPaths => "changed_paths",
    }
}

fn operation_cancellation(cancellation: OperationCancellation) -> ReindexCancelResponse {
    ReindexCancelResponse {
        operation_id: cancellation.operation_id,
        state: cancellation.state,
        cancelled: cancellation.cancelled,
    }
}

fn bounded_detail(value: String) -> String {
    const MAXIMUM: usize = 4 * 1024;
    crate::truncate_utf8(value, MAXIMUM)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::watch;
    use tokio::time::Instant;
    use unity_asset_search_protocol::{
        DaemonLifecycleState, DaemonLifecycleStatus, FreshnessMaintenance, GenerationFreshness,
        OperationId, QueryPolicyId, ReconcileLifecycle, ReindexOperationState, ServingAvailability,
        TimerLifecycleState, TimerStatus, ValidateContract, WatcherLifecycleState, WatcherStatus,
    };

    use super::{
        DispatcherShutdown, daemon_lifecycle_status, operation_status,
        requires_lifecycle_admission, tighten_shutdown,
    };
    use crate::coordinator::{
        ReindexAdmissionCounts, ReindexCoordinatorSnapshot, ReindexFailure, ReindexScopeKind,
    };
    use crate::lifecycle::AdmissionGate;
    use crate::operations::{OperationFailure, OperationOrigin, OperationSnapshot};
    use crate::watcher::{MaintenanceSnapshot, TimerLifecycle, WatcherLifecycle};

    fn query_policy() -> QueryPolicyId {
        QueryPolicyId::from_bytes([7; 32])
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
            background_reindex_operations: Vec::new(),
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
            daemon_lifecycle_status(false, &index_status, &coordinator, maintenance, Vec::new());
        assert_eq!(status.reconcile, ReconcileLifecycle::Idle);
        assert_eq!(status.lifecycle, DaemonLifecycleState::Serving);
    }

    #[test]
    fn draining_keeps_operation_observation_available() {
        assert!(!requires_lifecycle_admission(
            unity_asset_search_protocol::OperationKind::ReindexStatus,
        ));
        assert!(requires_lifecycle_admission(
            unity_asset_search_protocol::OperationKind::ReindexWait,
        ));
        assert!(requires_lifecycle_admission(
            unity_asset_search_protocol::OperationKind::ReindexCancel,
        ));
    }

    #[test]
    fn operation_adapter_preserves_terminal_evidence_and_query_policy() {
        let operation_id = OperationId::from_bytes([9; 16]);
        let status = operation_status(
            OperationSnapshot {
                origin: Some(OperationOrigin::WatcherOverflow),
                operation_id,
                state: ReindexOperationState::Failed,
                admission: None,
                completion: None,
                status: None,
                failure: Some(OperationFailure::Execution {
                    scope: ReindexScopeKind::Full,
                    message: "fixture failure".to_owned(),
                }),
            },
            query_policy(),
        );

        assert_eq!(status.operation_id, operation_id);
        assert_eq!(status.state, ReindexOperationState::Failed);
        assert_eq!(
            status.error.as_ref().unwrap().query_policy_id,
            Some(query_policy())
        );
        status.validate().unwrap();
    }

    #[test]
    fn shutdown_deadline_is_absolute_and_can_only_tighten() {
        let start = Instant::now();
        let later = start + Duration::from_secs(10);
        let earlier = start + Duration::from_secs(5);
        let mut current = None;

        assert!(tighten_shutdown(&mut current, later));
        assert_eq!(current, Some(later));
        assert!(!tighten_shutdown(
            &mut current,
            later + Duration::from_secs(1)
        ));
        assert_eq!(current, Some(later));
        assert!(tighten_shutdown(&mut current, earlier));
        assert_eq!(current, Some(earlier));
    }

    #[tokio::test]
    async fn shutdown_signal_closes_admission_before_publishing_deadline() {
        let admission = AdmissionGate::default();
        let (shutdown, receiver) = watch::channel(None);
        let handle = DispatcherShutdown {
            shutdown,
            admission: admission.clone(),
        };
        let deadline = Instant::now() + Duration::from_secs(1);

        handle.begin_shutdown_at(deadline);

        assert!(admission.admit().await.is_none());
        assert_eq!(*receiver.borrow(), Some(deadline));
    }
}
