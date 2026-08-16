//! Process-lifetime task ownership and daemon lifecycle primitives.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;
use unity_asset_search_index::{
    AssetLoadBudget, FilesystemReindexIntent, SearchIndex, SearchIndexError,
};
use unity_asset_search_local::{
    LoopbackEndpointClaim, LoopbackEndpointCleanup, PublishedLoopbackEndpoint,
};
use unity_asset_search_protocol::{DaemonInstanceId, ProjectId};

use crate::coordinator::{ReindexCoordinatorConfig, ReindexCoordinatorRuntime, ReindexExecution};
use crate::http_transport::{BoundLoopbackHttp, LoopbackHttpServer, LoopbackHttpServerError};
use crate::operations::{OperationOrigin, OperationServiceOwner, SemanticUpgradeRuntime};
use crate::service::{SearchService, SearchServiceShutdown};
use crate::watcher::{MaintenanceRuntime, WatcherConfig};

const DAEMON_RUNTIME_WORKER_THREADS: usize = 2;
const MAX_DAEMON_BLOCKING_TASKS: usize = 32;

#[derive(Clone, Default)]
pub struct AdmissionGate {
    state: Arc<AdmissionState>,
}

#[derive(Default)]
struct AdmissionState {
    closed: AtomicBool,
    linearization: Arc<tokio::sync::Mutex<()>>,
    failure: Mutex<Option<DaemonTaskFailure>>,
}

/// Process-lifetime daemon task whose unexpected termination invalidates serving authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonTaskKind {
    ReindexCoordinator,
    FilesystemWatcher,
    ReconcileTimer,
}

impl DaemonTaskKind {
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::ReindexCoordinator => "reindex_coordinator",
            Self::FilesystemWatcher => "filesystem_watcher",
            Self::ReconcileTimer => "reconcile_timer",
        }
    }
}

impl std::fmt::Display for DaemonTaskKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.wire_name())
    }
}

/// First process-lifetime task failure retained for admission and status decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonTaskFailure {
    pub(crate) task: DaemonTaskKind,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdmissionLifecycle {
    Serving,
    Draining,
    Failed(DaemonTaskFailure),
}

/// Linearization capability proving that work was accepted before draining began.
pub struct AdmissionPermit {
    _linearization: OwnedMutexGuard<()>,
}

impl AdmissionGate {
    pub async fn admit(&self) -> Option<AdmissionPermit> {
        if self.state.closed.load(Ordering::Acquire) {
            return None;
        }

        self.admit_after_open_observed().await
    }

    async fn admit_after_open_observed(&self) -> Option<AdmissionPermit> {
        let linearization = Arc::clone(&self.state.linearization).lock_owned().await;
        if self.state.closed.load(Ordering::Acquire) {
            return None;
        }

        Some(AdmissionPermit {
            _linearization: linearization,
        })
    }

    /// Prevents every admission that has not already linearized from entering daemon work.
    pub fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
    }

    /// Records the first process-lifetime task failure and closes every work admission boundary.
    pub(crate) fn fail_task(&self, task: DaemonTaskKind, message: impl Into<String>) {
        let mut message = crate::truncate_utf8(message.into(), 4 * 1024);
        if message.trim().is_empty() {
            message = "process-lifetime task terminated without diagnostic evidence".to_owned();
        }
        let mut failure = self
            .state
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if failure.is_none() {
            *failure = Some(DaemonTaskFailure { task, message });
        }
        self.close();
    }

    #[must_use]
    pub(crate) fn lifecycle(&self) -> AdmissionLifecycle {
        let failure = self
            .state
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match failure {
            Some(failure) => AdmissionLifecycle::Failed(failure),
            None if self.state.closed.load(Ordering::Acquire) => AdmissionLifecycle::Draining,
            None => AdmissionLifecycle::Serving,
        }
    }

    pub async fn wait_for_admission_linearization(&self) {
        debug_assert!(self.state.closed.load(Ordering::Acquire));
        let _linearization = self.state.linearization.lock().await;
    }

    #[cfg(test)]
    pub async fn is_draining(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }
}

/// The sole owner of resources whose destruction releases daemon or index-writer authority.
#[must_use = "the daemon runtime must be run through its complete shutdown sequence"]
pub struct DaemonRuntime {
    shutdown: SearchServiceShutdown,
    completion: Option<oneshot::Receiver<Result<DaemonShutdownReport, DaemonRuntimeError>>>,
}

/// Immutable inputs used to assemble the complete daemon resource graph.
pub struct DaemonRuntimeConfig {
    endpoint_claim: LoopbackEndpointClaim,
    daemon_instance_id: DaemonInstanceId,
    startup_reindex: Option<FilesystemReindexIntent>,
    index: SearchIndex,
    coordinator: ReindexCoordinatorConfig,
    watcher: Option<WatcherConfig>,
    reconcile_interval: Option<Duration>,
}

impl DaemonRuntimeConfig {
    #[must_use]
    pub fn new(
        endpoint_claim: LoopbackEndpointClaim,
        daemon_instance_id: DaemonInstanceId,
        index: SearchIndex,
        coordinator: ReindexCoordinatorConfig,
    ) -> Self {
        Self {
            endpoint_claim,
            daemon_instance_id,
            startup_reindex: None,
            index,
            coordinator,
            watcher: None,
            reconcile_interval: None,
        }
    }

    #[must_use]
    pub fn with_startup_reindex(mut self, intent: Option<FilesystemReindexIntent>) -> Self {
        self.startup_reindex = intent;
        self
    }

    #[must_use]
    pub fn with_watcher(mut self, watcher: Option<WatcherConfig>) -> Self {
        self.watcher = watcher;
        self
    }

    #[must_use]
    pub const fn with_reconcile_interval(mut self, interval: Option<Duration>) -> Self {
        self.reconcile_interval = interval;
        self
    }
}

struct DaemonRuntimeParts {
    endpoint_claim: LoopbackEndpointClaim,
    daemon_instance_id: DaemonInstanceId,
    startup_reindex: Option<FilesystemReindexIntent>,
    service: SearchService,
    http: LoopbackHttpServer,
    maintenance: MaintenanceRuntime,
    semantic_upgrade: SemanticUpgradeRuntime,
    coordinator: ReindexCoordinatorRuntime,
    operations: OperationServiceOwner,
    blocking_tasks: BlockingTaskOwner,
    index: SearchIndex,
    #[cfg(test)]
    panic_stage: Option<SupervisorPanicStage>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorPanicStage {
    BeforePublication,
    AfterPublication,
    DuringCleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonShutdownReport {
    pub endpoint_cleanup: LoopbackEndpointCleanup,
}

impl DaemonRuntime {
    /// Starts the complete daemon resource graph on a dedicated OS thread and Tokio runtime.
    ///
    /// The resource graph is assembled inside that runtime's context so every spawned owner is
    /// independent of the caller's runtime. This method returns only after synchronous shutdown
    /// control is available.
    pub fn start(config: DaemonRuntimeConfig) -> Result<Self, DaemonRuntimeError> {
        Self::start_with_factory(move || assemble_runtime(config))
    }

    fn start_with_factory<F, E>(factory: F) -> Result<Self, DaemonRuntimeError>
    where
        F: FnOnce() -> Result<DaemonRuntimeParts, E> + Send + 'static,
        E: std::fmt::Display,
    {
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let (completion_sender, completion) = oneshot::channel();
        let supervisor = thread::Builder::new()
            .name("unity-asset-search-supervisor".to_owned())
            .spawn(move || run_supervisor(factory, startup_sender, completion_sender))
            .map_err(|error| {
                DaemonRuntimeError::single(format!("start daemon supervisor thread: {error}"))
            })?;
        drop(supervisor);

        match startup_receiver.recv() {
            Ok(SupervisorStartup::Ready(shutdown)) => Ok(Self {
                shutdown,
                completion: Some(completion),
            }),
            Ok(SupervisorStartup::Failed(error)) => Err(error),
            Err(_) => Err(DaemonRuntimeError::single(
                "daemon supervisor terminated before startup completed",
            )),
        }
    }

    #[cfg(test)]
    pub fn begin_shutdown(&self, drain_timeout: Duration) {
        let deadline = Instant::now()
            .checked_add(drain_timeout)
            .expect("daemon shutdown duration fits Tokio Instant");
        self.shutdown.begin_shutdown_at(deadline);
    }

    /// Waits for the supervisor without transferring the completion receiver into the caller.
    ///
    /// If this future is cancelled, the same runtime can be awaited again. Dropping the handle
    /// requests immediate shutdown but never joins or cancels the independent supervisor thread.
    pub async fn run(&mut self) -> Result<DaemonShutdownReport, DaemonRuntimeError> {
        let Some(completion) = self.completion.as_mut() else {
            return Err(DaemonRuntimeError::single(
                "daemon supervisor completion has already been consumed",
            ));
        };
        let result = completion.await;
        self.completion.take();
        match result {
            Ok(result) => result,
            Err(_) => Err(DaemonRuntimeError::single(
                "daemon supervisor terminated unexpectedly",
            )),
        }
    }
}

fn assemble_runtime(
    config: DaemonRuntimeConfig,
) -> Result<DaemonRuntimeParts, DaemonAssemblyError> {
    let DaemonRuntimeConfig {
        endpoint_claim,
        daemon_instance_id,
        startup_reindex,
        index,
        coordinator,
        watcher,
        reconcile_interval,
    } = config;
    index.paths().revalidate_project_root().map_err(|source| {
        DaemonAssemblyError::ProjectAuthority {
            component: "search index",
            source,
        }
    })?;
    let endpoint_project_id = endpoint_claim.project_id();
    validate_project_binding("search index", endpoint_project_id, index.project_id())?;
    validate_project_binding(
        "reindex coordinator",
        endpoint_project_id,
        coordinator.project_id(),
    )?;
    if let Some(watcher) = watcher.as_ref() {
        watcher.paths.revalidate_project_root().map_err(|source| {
            DaemonAssemblyError::ProjectAuthority {
                component: "filesystem watcher",
                source,
            }
        })?;
        validate_project_binding(
            "filesystem watcher",
            endpoint_project_id,
            watcher.project_id(),
        )?;
    }
    let initial_status = index.status()?;
    let query_policy_id = initial_status.query_policy_id;
    let semantic_upgrade_required =
        initial_status
            .generation
            .active
            .as_ref()
            .is_some_and(|generation| {
                !generation.semantics_current || !generation.configuration_current
            });
    let bound_http = BoundLoopbackHttp::bind()?;
    let blocking_tasks = BlockingTaskOwner::new();
    let admission = AdmissionGate::default();
    let build_index = index.clone();
    let build_tasks = blocking_tasks.handle();
    let coordinator = ReindexCoordinatorRuntime::start_supervised(
        coordinator,
        admission.clone(),
        move |intent| {
            let index = build_index.clone();
            let tasks = build_tasks.clone();
            async move {
                let result = tasks
                    .run(
                        move || -> Result<_, unity_asset_search_index::SearchIndexError> {
                            let mut budget = AssetLoadBudget::default();
                            let receipt = index.reindex(intent, &mut budget)?;
                            let status = index.status()?;
                            Ok(ReindexExecution::new(receipt, status))
                        },
                    )
                    .await
                    .map_err(anyhow::Error::new)?;
                result.map_err(anyhow::Error::new)
            }
        },
    )?;
    let operations = OperationServiceOwner::new(
        daemon_instance_id,
        coordinator.coordinator(),
        admission.clone(),
    );
    let semantic_upgrade =
        SemanticUpgradeRuntime::start(semantic_upgrade_required, operations.service());
    let maintenance = MaintenanceRuntime::start(operations.service(), watcher, reconcile_interval);
    let service = SearchService::new(
        index.clone(),
        blocking_tasks.handle(),
        operations.service(),
        query_policy_id,
        admission,
        maintenance.handle(),
    );
    let http = bound_http.into_server(
        endpoint_project_id,
        daemon_instance_id,
        endpoint_claim.capability().clone(),
        blocking_tasks.handle(),
        service.clone(),
    );
    Ok(DaemonRuntimeParts {
        endpoint_claim,
        daemon_instance_id,
        startup_reindex,
        service,
        http,
        maintenance,
        semantic_upgrade,
        coordinator,
        operations,
        blocking_tasks,
        index,
        #[cfg(test)]
        panic_stage: None,
    })
}

fn validate_project_binding(
    component: &'static str,
    expected: ProjectId,
    actual: ProjectId,
) -> Result<(), DaemonAssemblyError> {
    if expected == actual {
        return Ok(());
    }
    Err(DaemonAssemblyError::ProjectMismatch {
        component,
        expected,
        actual,
    })
}

#[derive(Debug, thiserror::Error)]
enum DaemonAssemblyError {
    #[error("daemon project authority is invalid for {component}: {source}")]
    ProjectAuthority {
        component: &'static str,
        #[source]
        source: anyhow::Error,
    },
    #[error(
        "daemon project mismatch for {component}: endpoint project {expected:?}, component project {actual:?}"
    )]
    ProjectMismatch {
        component: &'static str,
        expected: ProjectId,
        actual: ProjectId,
    },
    #[error("inspect search index before daemon assembly: {0}")]
    SearchIndex(#[from] SearchIndexError),
    #[error("start reindex coordinator: {0}")]
    Coordinator(#[from] crate::coordinator::CoordinatorError),
    #[error("bind loopback HTTP listener: {0}")]
    Http(#[from] LoopbackHttpServerError),
}

impl Drop for DaemonRuntime {
    fn drop(&mut self) {
        if self.completion.is_some() {
            self.shutdown.begin_shutdown_at(Instant::now());
        }
    }
}

enum SupervisorStartup {
    Ready(SearchServiceShutdown),
    Failed(DaemonRuntimeError),
}

fn run_supervisor<F, E>(
    factory: F,
    startup: mpsc::SyncSender<SupervisorStartup>,
    completion: oneshot::Sender<Result<DaemonShutdownReport, DaemonRuntimeError>>,
) where
    F: FnOnce() -> Result<DaemonRuntimeParts, E>,
    E: std::fmt::Display,
{
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(DAEMON_RUNTIME_WORKER_THREADS)
        .enable_all()
        .thread_name("unity-asset-search-runtime")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = startup.send(SupervisorStartup::Failed(DaemonRuntimeError::single(
                format!("create daemon Tokio runtime: {error}"),
            )));
            return;
        }
    };

    let parts = {
        let _runtime_context = runtime.enter();
        catch_unwind(AssertUnwindSafe(factory))
    };
    let parts = match parts {
        Ok(Ok(parts)) => parts,
        Ok(Err(error)) => {
            let _ = startup.send(SupervisorStartup::Failed(DaemonRuntimeError::single(
                format!("assemble daemon runtime: {error}"),
            )));
            return;
        }
        Err(_) => {
            let _ = startup.send(SupervisorStartup::Failed(DaemonRuntimeError::single(
                "daemon runtime assembly panicked",
            )));
            return;
        }
    };

    let mut state = SupervisorState::from(parts);
    let shutdown = state.shutdown_handle();
    if startup
        .send(SupervisorStartup::Ready(shutdown.clone()))
        .is_err()
    {
        shutdown.begin_shutdown_at(Instant::now());
    }

    // State remains outside the unwind boundary. A panic may destroy an in-flight server future,
    // but it cannot drop the endpoint or writer leases before their blocking work is joined.
    let mut failures = match catch_unwind(AssertUnwindSafe(|| runtime.block_on(state.run()))) {
        Ok(Ok(())) => Vec::new(),
        Ok(Err(error)) => error.failures,
        Err(_) => vec!["daemon supervisor panicked".to_owned()],
    };
    let cleanup = catch_unwind(AssertUnwindSafe(|| runtime.block_on(state.shutdown())));
    match cleanup {
        Ok(cleanup_failures) => {
            failures.extend(cleanup_failures);
            let endpoint_cleanup = state.endpoint_cleanup();
            state.release_authority();
            drop(runtime);
            let _caller_was_dropped = completion.send(finish_shutdown(endpoint_cleanup, failures));
        }
        Err(_) => {
            // Releasing a lease after a panicked join path could admit a second writer while a
            // non-cancellable blocking task remains live. Retain authority until process exit.
            std::mem::forget(state);
            drop(runtime);
            let _caller_was_dropped = completion.send(Err(DaemonRuntimeError::single(
                "daemon supervisor cleanup panicked; authority remains retained until process exit",
            )));
        }
    }
}

struct RuntimeComponents {
    maintenance: MaintenanceRuntime,
    semantic_upgrade: SemanticUpgradeRuntime,
    coordinator: ReindexCoordinatorRuntime,
    operations: OperationServiceOwner,
    blocking_tasks: BlockingTaskOwner,
    index: SearchIndex,
}

impl RuntimeComponents {
    fn revalidate_project_root(
        &self,
        publication_boundary: &'static str,
    ) -> Result<(), DaemonRuntimeError> {
        self.index
            .paths()
            .revalidate_project_root()
            .map_err(|error| {
                DaemonRuntimeError::single(format!(
                    "project authority {publication_boundary}: {error:#}"
                ))
            })
    }

    async fn shutdown(&mut self) -> Vec<String> {
        let maintenance_result = self.maintenance.shutdown().await;
        let semantic_upgrade_result = self.semantic_upgrade.shutdown().await;
        let coordinator_result = self.coordinator.shutdown().await;
        let operation_result = self.operations.shutdown().await;
        let blocking_result = self.blocking_tasks.shutdown().await;

        let mut failures = Vec::new();
        collect_shutdown_failure(&mut failures, "maintenance", maintenance_result);
        collect_shutdown_failure(&mut failures, "semantic upgrade", semantic_upgrade_result);
        collect_shutdown_failure(&mut failures, "coordinator", coordinator_result);
        collect_shutdown_failure(&mut failures, "operation service", operation_result);
        collect_shutdown_failure(&mut failures, "blocking tasks", blocking_result);
        failures
    }
}

/// Owns authority for the full supervisor lifetime, outside every fallible serving future.
struct SupervisorState {
    endpoint_claim: LoopbackEndpointClaim,
    endpoint: Option<PublishedLoopbackEndpoint>,
    endpoint_cleanup: Option<LoopbackEndpointCleanup>,
    daemon_instance_id: DaemonInstanceId,
    startup_reindex: Option<FilesystemReindexIntent>,
    service: SearchService,
    http: LoopbackHttpServer,
    components: RuntimeComponents,
    #[cfg(test)]
    panic_stage: Option<SupervisorPanicStage>,
}

impl From<DaemonRuntimeParts> for SupervisorState {
    fn from(parts: DaemonRuntimeParts) -> Self {
        let DaemonRuntimeParts {
            endpoint_claim,
            daemon_instance_id,
            startup_reindex,
            service,
            http,
            maintenance,
            semantic_upgrade,
            coordinator,
            operations,
            blocking_tasks,
            index,
            #[cfg(test)]
            panic_stage,
        } = parts;
        Self {
            endpoint_claim,
            endpoint: None,
            endpoint_cleanup: None,
            daemon_instance_id,
            startup_reindex,
            service,
            http,
            components: RuntimeComponents {
                maintenance,
                semantic_upgrade,
                coordinator,
                operations,
                blocking_tasks,
                index,
            },
            #[cfg(test)]
            panic_stage,
        }
    }
}

impl SupervisorState {
    fn shutdown_handle(&self) -> SearchServiceShutdown {
        self.service.shutdown_handle()
    }

    async fn run(&mut self) -> Result<(), DaemonRuntimeError> {
        if self.service.requested_shutdown_deadline().is_some() {
            return Ok(());
        }

        self.components
            .semantic_upgrade
            .ensure_first_admission()
            .await
            .map_err(|error| {
                DaemonRuntimeError::single(format!("semantic upgrade admission: {error:#}"))
            })?;

        if let Some(intent) = self.startup_reindex.take()
            && let Err(error) = self
                .components
                .operations
                .service()
                .admit(OperationOrigin::Startup, intent, None)
                .await
        {
            return Err(DaemonRuntimeError::single(format!(
                "startup reindex admission: {error}"
            )));
        }

        #[cfg(test)]
        self.panic_if_requested(SupervisorPanicStage::BeforePublication);

        self.components
            .revalidate_project_root("before endpoint publication")?;
        let endpoint = self
            .endpoint_claim
            .publish(
                self.daemon_instance_id,
                self.http.address().port(),
                self.service.query_policy_id(),
            )
            .map_err(|error| {
                DaemonRuntimeError::single(format!("endpoint publication: {error}"))
            })?;
        self.endpoint = Some(endpoint);
        self.components
            .revalidate_project_root("after endpoint publication")?;
        if self
            .endpoint
            .as_ref()
            .expect("published endpoint is retained before revalidation")
            .publication_warning()
            .durability_unconfirmed()
        {
            eprintln!(
                "endpoint descriptor published but directory durability could not be confirmed"
            );
        }
        #[cfg(test)]
        self.panic_if_requested(SupervisorPanicStage::AfterPublication);

        let mut shutdown = self.service.subscribe_shutdown();
        if shutdown.borrow().is_some() {
            return Ok(());
        }
        tokio::select! {
            result = self.http.wait_for_exit() => {
                if self.service.requested_shutdown_deadline().is_some() {
                    return result.map_err(|error| {
                        DaemonRuntimeError::single(format!("loopback HTTP server: {error}"))
                    });
                }
                Err(match result {
                    Ok(()) => DaemonRuntimeError::single(
                        "loopback HTTP server exited before shutdown was requested",
                    ),
                    Err(error) => DaemonRuntimeError::single(format!(
                        "loopback HTTP server: {error}"
                    )),
                })
            }
            requested = shutdown.wait_for(Option::is_some) => {
                requested.map(|_| ()).map_err(|_| DaemonRuntimeError::single(
                    "loopback HTTP shutdown channel closed unexpectedly",
                ))
            }
        }
    }

    async fn shutdown(&mut self) -> Vec<String> {
        if self.service.requested_shutdown_deadline().is_none() {
            self.service.begin_shutdown_at(Instant::now());
        }
        #[cfg(test)]
        self.panic_if_requested(SupervisorPanicStage::DuringCleanup);
        let mut failures = Vec::new();
        collect_shutdown_failure(&mut failures, "endpoint cleanup", self.withdraw_endpoint());
        let http_result = self
            .http
            .shutdown_until(self.service.subscribe_shutdown())
            .await;
        collect_shutdown_failure(&mut failures, "loopback HTTP requests", http_result);
        self.service.wait_for_admission_linearization().await;
        failures.extend(self.components.shutdown().await);
        failures
    }

    fn endpoint_cleanup(&self) -> LoopbackEndpointCleanup {
        self.endpoint_cleanup
            .unwrap_or(LoopbackEndpointCleanup::AlreadyAbsent)
    }

    fn withdraw_endpoint(
        &mut self,
    ) -> Result<LoopbackEndpointCleanup, unity_asset_search_local::EndpointStoreError> {
        let cleanup = match self.endpoint.as_mut() {
            Some(endpoint) => endpoint.withdraw(),
            None => Ok(LoopbackEndpointCleanup::AlreadyAbsent),
        }?;
        self.record_endpoint_cleanup(cleanup);
        Ok(cleanup)
    }

    fn record_endpoint_cleanup(&mut self, cleanup: LoopbackEndpointCleanup) {
        if cleanup == LoopbackEndpointCleanup::Removed || self.endpoint_cleanup.is_none() {
            self.endpoint_cleanup = Some(cleanup);
        }
    }

    fn release_authority(self) {
        let Self {
            components,
            service,
            http,
            endpoint_claim,
            endpoint,
            ..
        } = self;
        // SearchIndex clones and every task owner release before either daemon lease holder.
        drop(components);
        drop(http);
        drop(service);
        drop(endpoint_claim);
        drop(endpoint);
    }

    #[cfg(test)]
    fn panic_if_requested(&self, stage: SupervisorPanicStage) {
        if self.panic_stage == Some(stage) {
            panic!("test-injected supervisor panic at {stage:?}");
        }
    }
}

fn finish_shutdown(
    endpoint_cleanup: LoopbackEndpointCleanup,
    failures: Vec<String>,
) -> Result<DaemonShutdownReport, DaemonRuntimeError> {
    if failures.is_empty() {
        Ok(DaemonShutdownReport { endpoint_cleanup })
    } else {
        Err(DaemonRuntimeError { failures })
    }
}

fn collect_shutdown_failure<T, E>(
    failures: &mut Vec<String>,
    component: &'static str,
    result: Result<T, E>,
) where
    E: std::fmt::Display,
{
    if let Err(error) = result {
        failures.push(format!("{component}: {error}"));
    }
}

#[derive(Debug)]
pub struct DaemonRuntimeError {
    failures: Vec<String>,
}

impl std::fmt::Display for DaemonRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("daemon runtime failed: ")?;
        for (index, failure) in self.failures.iter().enumerate() {
            if index != 0 {
                formatter.write_str("; ")?;
            }
            formatter.write_str(failure)?;
        }
        Ok(())
    }
}

impl std::error::Error for DaemonRuntimeError {}

impl DaemonRuntimeError {
    fn single(failure: impl Into<String>) -> Self {
        Self {
            failures: vec![failure.into()],
        }
    }
}

/// The unique owner of blocking work submitted by daemon components.
///
/// A cancelled request only drops its result receiver. The blocking task remains in this owner and
/// must be joined before index and endpoint leases are released.
#[must_use = "blocking tasks must be closed and joined before daemon leases release"]
pub struct BlockingTaskOwner {
    shared: Arc<BlockingTaskShared>,
    draining: Option<JoinSet<()>>,
}

#[derive(Clone)]
pub struct BlockingTaskHandle {
    shared: Arc<BlockingTaskShared>,
}

struct BlockingTaskShared {
    state: Mutex<BlockingTaskState>,
    capacity: Arc<Semaphore>,
}

struct BlockingTaskState {
    accepting: bool,
    tasks: JoinSet<()>,
}

impl BlockingTaskOwner {
    pub fn new() -> Self {
        Self::with_capacity(MAX_DAEMON_BLOCKING_TASKS)
    }

    fn with_capacity(maximum: usize) -> Self {
        Self {
            shared: Arc::new(BlockingTaskShared {
                state: Mutex::new(BlockingTaskState {
                    accepting: true,
                    tasks: JoinSet::new(),
                }),
                capacity: Arc::new(Semaphore::new(maximum)),
            }),
            draining: None,
        }
    }

    #[must_use]
    pub fn handle(&self) -> BlockingTaskHandle {
        BlockingTaskHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Atomically closes submission and joins every task, including results abandoned by callers.
    ///
    /// The join state remains in `self` across cancellation, so a later call resumes the same drain.
    pub async fn shutdown(&mut self) -> Result<(), BlockingTaskError> {
        if self.draining.is_none() {
            let mut state = self
                .shared
                .state
                .lock()
                .map_err(|_| BlockingTaskError::OwnerPoisoned)?;
            state.accepting = false;
            self.draining = Some(std::mem::replace(&mut state.tasks, JoinSet::new()));
        }

        let draining = self
            .draining
            .as_mut()
            .expect("blocking shutdown initialized its drain set");
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
            Some(error) => Err(BlockingTaskError::TaskTerminated(error)),
            None => Ok(()),
        }
    }
}

impl Default for BlockingTaskOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockingTaskHandle {
    pub async fn run<T, F>(&self, operation: F) -> Result<T, BlockingTaskError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        {
            let state = self
                .shared
                .state
                .lock()
                .map_err(|_| BlockingTaskError::OwnerPoisoned)?;
            if !state.accepting {
                return Err(BlockingTaskError::ShuttingDown);
            }
        }
        let capacity = Arc::clone(&self.shared.capacity)
            .acquire_owned()
            .await
            .expect("blocking task capacity remains open");
        let (result_sender, result_receiver) = oneshot::channel();
        {
            let mut state = self
                .shared
                .state
                .lock()
                .map_err(|_| BlockingTaskError::OwnerPoisoned)?;
            if !state.accepting {
                return Err(BlockingTaskError::ShuttingDown);
            }
            while state.tasks.try_join_next().is_some() {}
            state.tasks.spawn_blocking(move || {
                let _capacity: OwnedSemaphorePermit = capacity;
                let result = catch_unwind(AssertUnwindSafe(operation))
                    .map_err(|_| BlockingTaskError::Panicked);
                let _receiver_was_cancelled = result_sender.send(result);
            });
        }
        result_receiver
            .await
            .map_err(|_| BlockingTaskError::ResultChannelClosed)?
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BlockingTaskError {
    #[error("blocking task owner is shutting down")]
    ShuttingDown,
    #[error("blocking task panicked")]
    Panicked,
    #[error("blocking task result channel closed unexpectedly")]
    ResultChannelClosed,
    #[error("blocking task owner state is poisoned")]
    OwnerPoisoned,
    #[error("blocking task terminated unexpectedly: {0}")]
    TaskTerminated(String),
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use tokio::sync::oneshot;
    use unity_asset_search_index::{AssetLoadBudget, IndexPaths, SearchIndex};
    use unity_asset_search_local::{
        EndpointNamespaceV1, EndpointStoreError, LoopbackEndpointClaim, PrivateRootsV1,
        ProjectLocatorV1, generate_daemon_instance_id,
    };
    use unity_asset_search_protocol::DaemonInstanceId;

    use super::{
        AdmissionGate, AdmissionLifecycle, BlockingTaskError, BlockingTaskOwner,
        DaemonAssemblyError, DaemonRuntime, DaemonRuntimeConfig, DaemonRuntimeParts,
        DaemonTaskKind, SupervisorPanicStage, assemble_runtime,
    };
    use crate::coordinator::{
        ReindexCoordinatorConfig, ReindexCoordinatorRuntime, ReindexExecution,
    };
    use crate::http_transport::BoundLoopbackHttp;
    use crate::operations::{OperationServiceOwner, SemanticUpgradeRuntime};
    use crate::service::SearchService;
    use crate::watcher::{MaintenanceRuntime, WatcherConfig};

    #[derive(Debug, Clone, Copy)]
    enum ForeignRuntimeComponent {
        SearchIndex,
        Coordinator,
        Watcher,
    }

    impl ForeignRuntimeComponent {
        const fn label(self) -> &'static str {
            match self {
                Self::SearchIndex => "search index",
                Self::Coordinator => "reindex coordinator",
                Self::Watcher => "filesystem watcher",
            }
        }
    }

    #[test]
    fn runtime_assembly_rejects_foreign_components_before_endpoint_publication() {
        for component in [
            ForeignRuntimeComponent::SearchIndex,
            ForeignRuntimeComponent::Coordinator,
            ForeignRuntimeComponent::Watcher,
        ] {
            assert_runtime_assembly_rejects_foreign_component(component);
        }
    }

    fn assert_runtime_assembly_rejects_foreign_component(component: ForeignRuntimeComponent) {
        let endpoint_project = crate::secure_test_tempdir();
        fs::create_dir(endpoint_project.path().join("Assets")).unwrap();
        fs::create_dir(endpoint_project.path().join("ProjectSettings")).unwrap();
        let endpoint_locator = ProjectLocatorV1::open(endpoint_project.path()).unwrap();

        let foreign_project = crate::secure_test_tempdir();
        fs::create_dir(foreign_project.path().join("Assets")).unwrap();
        fs::create_dir(foreign_project.path().join("ProjectSettings")).unwrap();
        let foreign_locator = ProjectLocatorV1::open(foreign_project.path()).unwrap();

        let roots = PrivateRootsV1::discover_for_current_context().unwrap();
        let namespace = roots
            .runtime()
            .endpoint_namespace(endpoint_locator.project_id())
            .unwrap();
        let cleanup_path = namespace.path().to_path_buf();
        let endpoint_claim = namespace.claim_loopback_endpoint().unwrap();
        let endpoint_paths = IndexPaths::for_project(
            endpoint_locator.root().to_path_buf(),
            Some(endpoint_project.path().join(".endpoint-index")),
            None,
        )
        .unwrap();
        let foreign_paths = IndexPaths::for_project(
            foreign_locator.root().to_path_buf(),
            Some(foreign_project.path().join(".foreign-index")),
            None,
        )
        .unwrap();
        let index_paths = match component {
            ForeignRuntimeComponent::SearchIndex => &foreign_paths,
            ForeignRuntimeComponent::Coordinator | ForeignRuntimeComponent::Watcher => {
                &endpoint_paths
            }
        };
        let coordinator_paths = match component {
            ForeignRuntimeComponent::Coordinator => &foreign_paths,
            ForeignRuntimeComponent::SearchIndex | ForeignRuntimeComponent::Watcher => {
                &endpoint_paths
            }
        };
        let mut budget = AssetLoadBudget::default();
        let index = SearchIndex::open_or_create((*index_paths).clone(), &mut budget).unwrap();
        let watcher =
            matches!(component, ForeignRuntimeComponent::Watcher).then(|| WatcherConfig {
                paths: foreign_paths.clone(),
            });
        let config = DaemonRuntimeConfig::new(
            endpoint_claim,
            generate_daemon_instance_id().unwrap(),
            index,
            ReindexCoordinatorConfig::new(coordinator_paths.project_path_space().clone()),
        )
        .with_watcher(watcher);

        let error = match assemble_runtime(config) {
            Ok(_) => {
                panic!("foreign {component:?} unexpectedly assembled into the endpoint runtime")
            }
            Err(error) => error,
        };
        assert!(matches!(
            error,
            DaemonAssemblyError::ProjectMismatch {
                component: actual_component,
                expected,
                actual,
            } if expected == endpoint_locator.project_id()
                && actual == foreign_locator.project_id()
                && actual_component == component.label()
        ));
        assert!(matches!(
            namespace.discover_loopback_endpoint(),
            Err(EndpointStoreError::DescriptorMissing)
        ));
        let replacement = namespace.claim_loopback_endpoint().unwrap();
        drop(replacement);
        drop(namespace);
        drop(roots);
        for name in ["binding.v1", ".binding-v1.lock", ".daemon-v1.lock"] {
            let result = fs::remove_file(cleanup_path.join(name));
            assert!(
                result.is_ok()
                    || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            );
        }
        fs::remove_dir(cleanup_path).unwrap();
    }

    #[tokio::test]
    async fn synchronous_close_rejects_admission_without_waiting_for_an_existing_permit() {
        let gate = AdmissionGate::default();
        let _existing = gate.admit().await.unwrap();

        gate.close();

        assert!(gate.admit().await.is_none());
        assert!(gate.is_draining().await);
    }

    #[tokio::test]
    async fn admission_rechecks_close_after_waiting_for_the_linearization_permit() {
        let gate = AdmissionGate::default();
        let existing = gate.admit().await.unwrap();
        let waiting_gate = gate.clone();
        let (started_sender, started_receiver) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            started_sender.send(()).unwrap();
            waiting_gate.admit_after_open_observed().await
        });
        started_receiver.await.unwrap();

        gate.close();
        drop(existing);

        assert!(waiter.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn first_task_failure_closes_admission_and_remains_the_lifecycle_authority() {
        let gate = AdmissionGate::default();

        gate.fail_task(
            DaemonTaskKind::ReindexCoordinator,
            "coordinator runner panicked",
        );
        gate.fail_task(DaemonTaskKind::FilesystemWatcher, "watcher followed it");

        assert!(gate.admit().await.is_none());
        assert_eq!(
            gate.lifecycle(),
            AdmissionLifecycle::Failed(super::DaemonTaskFailure {
                task: DaemonTaskKind::ReindexCoordinator,
                message: "coordinator runner panicked".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn cancelled_caller_cannot_detach_blocking_work_from_owner() {
        let mut owner = BlockingTaskOwner::new();
        let handle = owner.handle();
        let (started_sender, started_receiver) = oneshot::channel();
        let (finish_sender, finish_receiver) = mpsc::channel();
        let caller = tokio::spawn(async move {
            handle
                .run(move || {
                    let _ = started_sender.send(());
                    finish_receiver.recv().unwrap();
                })
                .await
        });
        started_receiver.await.unwrap();
        caller.abort();
        let mut shutdown = Box::pin(owner.shutdown());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut shutdown)
                .await
                .is_err()
        );

        finish_sender.send(()).unwrap();
        shutdown.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_caller_cannot_release_blocking_capacity_early() {
        let mut owner = BlockingTaskOwner::with_capacity(1);
        let first_handle = owner.handle();
        let (first_started_sender, first_started_receiver) = oneshot::channel();
        let (first_finish_sender, first_finish_receiver) = mpsc::channel();
        let first_caller = tokio::spawn(async move {
            first_handle
                .run(move || {
                    let _ = first_started_sender.send(());
                    first_finish_receiver.recv().unwrap();
                })
                .await
        });
        first_started_receiver.await.unwrap();
        first_caller.abort();

        let second_handle = owner.handle();
        let (second_started_sender, second_started_receiver) = oneshot::channel();
        let second_caller = tokio::spawn(async move {
            second_handle
                .run(move || {
                    let _ = second_started_sender.send(());
                    2_u8
                })
                .await
        });
        let mut second_started_receiver = Box::pin(second_started_receiver);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                &mut second_started_receiver,
            )
            .await
            .is_err()
        );

        first_finish_sender.send(()).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            &mut second_started_receiver,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second_caller.await.unwrap().unwrap(), 2);
        owner.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn closed_owner_rejects_new_work() {
        let mut owner = BlockingTaskOwner::new();
        let handle = owner.handle();
        owner.shutdown().await.unwrap();

        assert!(matches!(
            handle.run(|| 1_u8).await,
            Err(BlockingTaskError::ShuttingDown)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_runtime_wait_retains_both_leases_until_blocking_work_joins() {
        let mut fixture = RuntimeFixture::new(generate_daemon_instance_id().unwrap());
        fixture.wait_for_publication().await;
        fixture.runtime().begin_shutdown(Duration::ZERO);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), fixture.runtime_mut().run())
                .await
                .is_err()
        );
        fixture.assert_authority_is_still_held();

        fixture.release_blocking_work();
        let report = fixture.runtime_mut().run().await.unwrap();
        assert_eq!(
            report.endpoint_cleanup,
            unity_asset_search_local::LoopbackEndpointCleanup::Removed
        );
        fixture.finish();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_publication_failure_joins_work_before_releasing_authority() {
        let mut fixture = RuntimeFixture::new(DaemonInstanceId::from_bytes([0; 16]));

        assert!(
            tokio::time::timeout(Duration::from_millis(10), fixture.runtime_mut().run())
                .await
                .is_err()
        );
        fixture.assert_authority_is_still_held();

        fixture.release_blocking_work();
        let error = fixture.runtime_mut().run().await.unwrap_err();
        assert!(error.to_string().contains("endpoint publication"));
        fixture.finish();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervisor_panic_before_publication_joins_work_before_releasing_authority() {
        let mut fixture = RuntimeFixture::new_with_panic(
            generate_daemon_instance_id().unwrap(),
            SupervisorPanicStage::BeforePublication,
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(10), fixture.runtime_mut().run())
                .await
                .is_err()
        );
        fixture.assert_authority_is_still_held();

        fixture.release_blocking_work();
        let error = fixture.runtime_mut().run().await.unwrap_err();
        assert!(error.to_string().contains("supervisor panicked"));
        fixture.finish();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervisor_panic_after_publication_joins_work_before_releasing_authority() {
        let mut fixture = RuntimeFixture::new_with_panic(
            generate_daemon_instance_id().unwrap(),
            SupervisorPanicStage::AfterPublication,
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(10), fixture.runtime_mut().run())
                .await
                .is_err()
        );
        fixture.assert_authority_is_still_held();

        fixture.release_blocking_work();
        let error = fixture.runtime_mut().run().await.unwrap_err();
        assert!(error.to_string().contains("supervisor panicked"));
        fixture.finish();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cleanup_panic_retains_authority_until_process_exit() {
        let mut fixture = RuntimeFixture::new_with_panic(
            generate_daemon_instance_id().unwrap(),
            SupervisorPanicStage::DuringCleanup,
        );
        fixture.wait_for_publication().await;
        fixture.runtime().begin_shutdown(Duration::ZERO);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), fixture.runtime_mut().run())
                .await
                .is_err()
        );
        fixture.assert_authority_is_still_held();

        fixture.release_blocking_work();
        let error = fixture.runtime_mut().run().await.unwrap_err();
        assert!(error.to_string().contains("cleanup panicked"));
        fixture.assert_authority_is_still_held();

        // This branch intentionally retains authority until this test process exits. The fixture
        // uses unique project and endpoint roots, so preserving it cannot block another test.
        std::mem::forget(fixture);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_withdraws_discovery_before_waiting_for_prior_admission() {
        let mut fixture = RuntimeFixture::new(generate_daemon_instance_id().unwrap());
        fixture.wait_for_publication().await;
        let admission = fixture.admission().admit().await.unwrap();
        fixture.runtime().begin_shutdown(Duration::from_secs(5));

        assert!(
            tokio::time::timeout(Duration::from_secs(1), fixture.runtime_mut().run())
                .await
                .is_err(),
            "the retained admission must keep shutdown incomplete"
        );
        assert!(matches!(
            fixture.namespace().discover_loopback_endpoint(),
            Err(EndpointStoreError::DescriptorMissing)
        ));
        fixture.assert_authority_is_still_held();

        drop(admission);
        fixture.release_blocking_work();
        fixture.runtime_mut().run().await.unwrap();
        fixture.finish();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_runtime_detaches_cleanup_instead_of_aborting_it() {
        let mut fixture = RuntimeFixture::new(generate_daemon_instance_id().unwrap());
        fixture.wait_for_publication().await;

        drop(fixture.runtime.take());
        fixture.assert_authority_is_still_held();
        fixture.release_blocking_work();
        fixture.wait_for_authority_release();
        fixture.finish();
    }

    #[test]
    fn destroying_callers_tokio_runtime_cannot_cancel_supervisor_cleanup() {
        let mut fixture =
            fixture_after_caller_runtime_destroyed(generate_daemon_instance_id().unwrap(), true);

        fixture.assert_authority_is_still_held();
        fixture.release_blocking_work();
        fixture.wait_for_authority_release();
        fixture.finish();
    }

    #[test]
    fn publication_failure_survives_callers_tokio_runtime_destruction() {
        let mut fixture =
            fixture_after_caller_runtime_destroyed(DaemonInstanceId::from_bytes([0; 16]), false);

        fixture.assert_authority_is_still_held();
        fixture.release_blocking_work();
        fixture.wait_for_authority_release();
        fixture.finish();
    }

    struct RuntimeFixture {
        _project: tempfile::TempDir,
        namespace: Option<EndpointNamespaceV1>,
        cleanup_path: PathBuf,
        index_paths: IndexPaths,
        runtime: Option<DaemonRuntime>,
        admission: AdmissionGate,
        release: Option<mpsc::Sender<()>>,
    }

    impl RuntimeFixture {
        fn new(daemon_instance_id: DaemonInstanceId) -> Self {
            Self::new_with_optional_panic(daemon_instance_id, None)
        }

        fn new_with_panic(
            daemon_instance_id: DaemonInstanceId,
            panic_stage: SupervisorPanicStage,
        ) -> Self {
            Self::new_with_optional_panic(daemon_instance_id, Some(panic_stage))
        }

        fn new_with_optional_panic(
            daemon_instance_id: DaemonInstanceId,
            panic_stage: Option<SupervisorPanicStage>,
        ) -> Self {
            let project = crate::secure_test_tempdir();
            fs::create_dir(project.path().join("Assets")).unwrap();
            fs::create_dir(project.path().join("ProjectSettings")).unwrap();
            let locator = ProjectLocatorV1::open(project.path()).unwrap();
            let roots = PrivateRootsV1::discover_for_current_context().unwrap();
            let namespace = roots
                .runtime()
                .endpoint_namespace(locator.project_id())
                .unwrap();
            let cleanup_path = namespace.path().to_path_buf();
            let endpoint_claim = namespace.claim_loopback_endpoint().unwrap();
            let index_paths = IndexPaths::for_project(
                locator.root().to_path_buf(),
                Some(project.path().join(".lifecycle-index")),
                None,
            )
            .unwrap();
            let mut budget = AssetLoadBudget::default();
            let index = SearchIndex::open_or_create(index_paths.clone(), &mut budget).unwrap();
            let query_policy_id = index.status().unwrap().query_policy_id;

            let (started_sender, started_receiver) = mpsc::sync_channel(1);
            let (release_sender, release_receiver) = mpsc::channel();
            let (runtime, admission) = start_fixture_runtime(
                endpoint_claim,
                daemon_instance_id,
                query_policy_id,
                index,
                started_sender,
                release_receiver,
                panic_stage,
            );
            started_receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("fixture blocking work starts on the supervisor runtime");

            Self {
                _project: project,
                namespace: Some(namespace),
                cleanup_path,
                index_paths,
                runtime: Some(runtime),
                admission,
                release: Some(release_sender),
            }
        }

        async fn wait_for_publication(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.namespace().discover_loopback_endpoint() {
                    Ok(_) => return,
                    Err(EndpointStoreError::DescriptorMissing) => {}
                    Err(error) => panic!("wait for lifecycle endpoint publication: {error}"),
                }
                assert!(Instant::now() < deadline, "endpoint publication timed out");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        fn assert_authority_is_still_held(&self) {
            assert!(matches!(
                self.namespace().claim_loopback_endpoint(),
                Err(EndpointStoreError::LeaseHeld)
            ));
            let mut budget = AssetLoadBudget::default();
            assert!(
                SearchIndex::open_or_create(self.index_paths.clone(), &mut budget).is_err(),
                "runtime released the index writer lease before blocking work joined"
            );
        }

        fn release_blocking_work(&mut self) {
            self.release.take().unwrap().send(()).unwrap();
        }

        fn wait_for_authority_release(&self) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let replacement = loop {
                match self.namespace().claim_loopback_endpoint() {
                    Ok(claim) => break claim,
                    Err(EndpointStoreError::LeaseHeld) => {}
                    Err(error) => panic!("reacquire endpoint after detached shutdown: {error}"),
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "independent supervisor did not finish shutdown"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            let reopened = loop {
                let mut budget = AssetLoadBudget::default();
                match SearchIndex::open_or_create(self.index_paths.clone(), &mut budget) {
                    Ok(index) => break index,
                    Err(_) => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "independent supervisor retained the index writer lease"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            };
            drop(reopened);
            drop(replacement);
        }

        fn finish(mut self) {
            let replacement = self.namespace().claim_loopback_endpoint().unwrap();
            drop(replacement);
            let mut budget = AssetLoadBudget::default();
            let reopened =
                SearchIndex::open_or_create(self.index_paths.clone(), &mut budget).unwrap();
            drop(reopened);
            drop(self.runtime.take());
            drop(self.namespace.take());
            for name in ["binding.v1", ".binding-v1.lock", ".daemon-v1.lock"] {
                let result = fs::remove_file(self.cleanup_path.join(name));
                assert!(
                    result.is_ok()
                        || result.is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                );
            }
            fs::remove_dir(&self.cleanup_path).unwrap();
        }

        fn namespace(&self) -> &EndpointNamespaceV1 {
            self.namespace.as_ref().unwrap()
        }

        fn runtime(&self) -> &DaemonRuntime {
            self.runtime.as_ref().unwrap()
        }

        fn runtime_mut(&mut self) -> &mut DaemonRuntime {
            self.runtime.as_mut().unwrap()
        }

        fn admission(&self) -> &AdmissionGate {
            &self.admission
        }
    }

    fn fixture_after_caller_runtime_destroyed(
        daemon_instance_id: DaemonInstanceId,
        wait_for_publication: bool,
    ) -> RuntimeFixture {
        let (fixture_sender, fixture_receiver) = mpsc::sync_channel(1);
        let caller = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let fixture = runtime.block_on(async move {
                let mut fixture = RuntimeFixture::new(daemon_instance_id);
                if wait_for_publication {
                    fixture.wait_for_publication().await;
                }
                drop(fixture.runtime.take());
                fixture
            });
            runtime.shutdown_timeout(Duration::ZERO);
            fixture_sender.send(fixture).unwrap();
        });
        caller.join().unwrap();
        fixture_receiver.recv().unwrap()
    }

    fn start_fixture_runtime(
        endpoint_claim: LoopbackEndpointClaim,
        daemon_instance_id: DaemonInstanceId,
        query_policy_id: unity_asset_search_protocol::QueryPolicyId,
        index: SearchIndex,
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        panic_stage: Option<SupervisorPanicStage>,
    ) -> (DaemonRuntime, AdmissionGate) {
        let admission = AdmissionGate::default();
        let runtime_admission = admission.clone();
        let runtime = DaemonRuntime::start_with_factory(move || -> anyhow::Result<_> {
            let endpoint_project_id = endpoint_claim.project_id();
            let capability = endpoint_claim.capability().clone();
            let bound_http = BoundLoopbackHttp::bind()?;
            let blocking_tasks = BlockingTaskOwner::new();
            let blocking_handle = blocking_tasks.handle();
            drop(tokio::spawn(async move {
                let _ = blocking_handle
                    .run(move || {
                        let _ = started.send(());
                        let _ = release.recv();
                    })
                    .await;
            }));

            let coordinator = ReindexCoordinatorRuntime::start(
                ReindexCoordinatorConfig::new(index.paths().project_path_space().clone()),
                |_| async {
                    Err::<ReindexExecution, _>(anyhow::anyhow!(
                        "lifecycle fixture must not execute a reindex"
                    ))
                },
            )?;
            let operations = OperationServiceOwner::new(
                daemon_instance_id,
                coordinator.coordinator(),
                runtime_admission.clone(),
            );
            let semantic_upgrade = SemanticUpgradeRuntime::start(false, operations.service());
            let maintenance = MaintenanceRuntime::start(operations.service(), None, None);
            let service = SearchService::new(
                index.clone(),
                blocking_tasks.handle(),
                operations.service(),
                query_policy_id,
                runtime_admission,
                maintenance.handle(),
            );
            let http = bound_http.into_server(
                endpoint_project_id,
                daemon_instance_id,
                capability,
                blocking_tasks.handle(),
                service.clone(),
            );
            Ok(DaemonRuntimeParts {
                endpoint_claim,
                daemon_instance_id,
                startup_reindex: None,
                service,
                http,
                maintenance,
                semantic_upgrade,
                coordinator,
                operations,
                blocking_tasks,
                index,
                panic_stage,
            })
        })
        .unwrap();
        (runtime, admission)
    }

    impl Drop for RuntimeFixture {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
        }
    }
}
