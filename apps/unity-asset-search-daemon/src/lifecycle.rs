//! Process-lifetime task ownership and daemon lifecycle primitives.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use tokio::sync::{OwnedMutexGuard, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;
use unity_asset_search_index::{
    AssetLoadBudget, FilesystemReindexIntent, SearchIndex, SearchIndexError,
};
use unity_asset_search_local::{ClaimedEndpointV1, EndpointClaimV1, EndpointCleanupV1};
use unity_asset_search_protocol::DaemonInstanceId;

use crate::coordinator::{
    ReindexCoordinatorConfig, ReindexCoordinatorRuntime, ReindexExecution, ReindexSource,
};
use crate::ipc::{Dispatcher, DispatcherShutdown, IpcService, OperationRegistryOwner};
use crate::watcher::{MaintenanceRuntime, WatcherConfig};

const DAEMON_RUNTIME_WORKER_THREADS: usize = 2;

#[derive(Clone, Default)]
pub struct AdmissionGate {
    state: Arc<AdmissionState>,
}

#[derive(Default)]
struct AdmissionState {
    closed: AtomicBool,
    linearization: Arc<tokio::sync::Mutex<()>>,
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

    pub async fn begin_draining(&self) {
        self.close();
        let _linearization = self.state.linearization.lock().await;
    }

    pub async fn is_draining(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }
}

/// The sole owner of resources whose destruction releases daemon or index-writer authority.
#[must_use = "the daemon runtime must be run through its complete shutdown sequence"]
pub struct DaemonRuntime {
    shutdown: DispatcherShutdown,
    completion: Option<oneshot::Receiver<Result<DaemonShutdownReport, DaemonRuntimeError>>>,
}

/// Immutable inputs used to assemble the complete daemon resource graph.
pub struct DaemonRuntimeConfig {
    endpoint_claim: EndpointClaimV1,
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
        endpoint_claim: EndpointClaimV1,
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
    endpoint_claim: EndpointClaimV1,
    daemon_instance_id: DaemonInstanceId,
    startup_reindex: Option<FilesystemReindexIntent>,
    dispatcher: Dispatcher,
    maintenance: MaintenanceRuntime,
    coordinator: ReindexCoordinatorRuntime,
    operations: OperationRegistryOwner,
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
    AfterSessionSpawn,
    DuringCleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonShutdownReport {
    pub endpoint_cleanup: EndpointCleanupV1,
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
    let project_root = index.paths().project_root().to_path_buf();
    let query_policy_id = index.status()?.query_policy_id;
    let blocking_tasks = BlockingTaskOwner::new();
    let build_index = index.clone();
    let build_tasks = blocking_tasks.handle();
    let coordinator = ReindexCoordinatorRuntime::start(coordinator, move |intent| {
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
    })?;
    let admission = AdmissionGate::default();
    let operations = OperationRegistryOwner::new(
        daemon_instance_id,
        coordinator.coordinator(),
        project_root,
        query_policy_id,
        admission.clone(),
    );
    let maintenance = MaintenanceRuntime::start(operations.registry(), watcher, reconcile_interval);
    let dispatcher = Dispatcher::new(
        index.clone(),
        blocking_tasks.handle(),
        operations.registry(),
        admission,
        maintenance.handle(),
    );
    Ok(DaemonRuntimeParts {
        endpoint_claim,
        daemon_instance_id,
        startup_reindex,
        dispatcher,
        maintenance,
        coordinator,
        operations,
        blocking_tasks,
        index,
        #[cfg(test)]
        panic_stage: None,
    })
}

#[derive(Debug, thiserror::Error)]
enum DaemonAssemblyError {
    #[error("inspect search index before daemon assembly: {0}")]
    SearchIndex(#[from] SearchIndexError),
    #[error("start reindex coordinator: {0}")]
    Coordinator(#[from] crate::coordinator::CoordinatorError),
}

impl Drop for DaemonRuntime {
    fn drop(&mut self) {
        if self.completion.is_some() {
            self.shutdown.begin_shutdown_at(Instant::now());
        }
    }
}

enum SupervisorStartup {
    Ready(DispatcherShutdown),
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
    coordinator: ReindexCoordinatorRuntime,
    operations: OperationRegistryOwner,
    blocking_tasks: BlockingTaskOwner,
    _index: SearchIndex,
}

impl RuntimeComponents {
    async fn shutdown(&mut self) -> Vec<String> {
        let maintenance_result = self.maintenance.shutdown().await;
        let coordinator_result = self.coordinator.shutdown().await;
        let operation_result = self.operations.shutdown().await;
        let blocking_result = self.blocking_tasks.shutdown().await;

        let mut failures = Vec::new();
        collect_shutdown_failure(&mut failures, "maintenance", maintenance_result);
        collect_shutdown_failure(&mut failures, "coordinator", coordinator_result);
        collect_shutdown_failure(&mut failures, "operation registry", operation_result);
        collect_shutdown_failure(&mut failures, "blocking tasks", blocking_result);
        failures
    }
}

/// Owns authority for the full supervisor lifetime, outside every fallible serving future.
struct SupervisorState {
    endpoint_claim: EndpointClaimV1,
    endpoint: Option<ClaimedEndpointV1>,
    endpoint_cleanup: Option<EndpointCleanupV1>,
    daemon_instance_id: DaemonInstanceId,
    startup_reindex: Option<FilesystemReindexIntent>,
    ipc: IpcService,
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
            dispatcher,
            maintenance,
            coordinator,
            operations,
            blocking_tasks,
            index,
            #[cfg(test)]
            panic_stage,
        } = parts;
        let ipc = IpcService::new(dispatcher);
        #[cfg(test)]
        let ipc = ipc.with_panic_after_session_spawn(
            panic_stage == Some(SupervisorPanicStage::AfterSessionSpawn),
        );
        Self {
            endpoint_claim,
            endpoint: None,
            endpoint_cleanup: None,
            daemon_instance_id,
            startup_reindex,
            ipc,
            components: RuntimeComponents {
                maintenance,
                coordinator,
                operations,
                blocking_tasks,
                _index: index,
            },
            #[cfg(test)]
            panic_stage,
        }
    }
}

impl SupervisorState {
    fn shutdown_handle(&self) -> DispatcherShutdown {
        self.ipc.shutdown_handle()
    }

    async fn run(&mut self) -> Result<(), DaemonRuntimeError> {
        if self.ipc.requested_shutdown_deadline().is_some() {
            return Ok(());
        }

        if let Some(intent) = self.startup_reindex.take()
            && let Err(error) = self
                .components
                .operations
                .registry()
                .admit_internal(ReindexSource::Startup, intent)
                .await
        {
            return Err(DaemonRuntimeError::single(format!(
                "startup reindex admission: {error:?}"
            )));
        }

        #[cfg(test)]
        self.panic_if_requested(SupervisorPanicStage::BeforePublication);

        let endpoint = self
            .endpoint_claim
            .publish(self.daemon_instance_id)
            .map_err(|error| {
                DaemonRuntimeError::single(format!("endpoint publication: {error}"))
            })?;
        if endpoint.publication_warning().durability_unconfirmed() {
            eprintln!(
                "endpoint descriptor published but directory durability could not be confirmed"
            );
        }
        self.endpoint = Some(endpoint);

        #[cfg(test)]
        self.panic_if_requested(SupervisorPanicStage::AfterPublication);

        let serve_result = {
            let endpoint = self
                .endpoint
                .as_mut()
                .expect("supervisor retains a published endpoint before serving");
            self.ipc.serve(endpoint).await
        };
        match serve_result {
            Ok(cleanup) => {
                self.record_endpoint_cleanup(cleanup);
                Ok(())
            }
            Err(error) => Err(DaemonRuntimeError::single(format!("IPC server: {error:#}"))),
        }
    }

    async fn shutdown(&mut self) -> Vec<String> {
        self.ipc.begin_shutdown_at(Instant::now());
        #[cfg(test)]
        self.panic_if_requested(SupervisorPanicStage::DuringCleanup);
        let mut failures = Vec::new();
        if let Err(error) = self.withdraw_endpoint() {
            failures.push(format!("endpoint cleanup: {error}"));
        }
        self.ipc.shutdown().await;
        failures.extend(self.components.shutdown().await);
        failures
    }

    fn endpoint_cleanup(&self) -> EndpointCleanupV1 {
        self.endpoint_cleanup
            .unwrap_or(EndpointCleanupV1::AlreadyAbsent)
    }

    fn withdraw_endpoint(
        &mut self,
    ) -> Result<EndpointCleanupV1, unity_asset_search_local::EndpointStoreError> {
        let cleanup = match self.endpoint.as_mut() {
            Some(endpoint) => endpoint.withdraw(),
            None => Ok(EndpointCleanupV1::AlreadyAbsent),
        }?;
        self.record_endpoint_cleanup(cleanup);
        Ok(cleanup)
    }

    fn record_endpoint_cleanup(&mut self, cleanup: EndpointCleanupV1) {
        if cleanup == EndpointCleanupV1::Removed || self.endpoint_cleanup.is_none() {
            self.endpoint_cleanup = Some(cleanup);
        }
    }

    fn release_authority(self) {
        let Self {
            components,
            ipc,
            endpoint_claim,
            endpoint,
            ..
        } = self;
        // SearchIndex clones and every task owner release before either daemon lease holder.
        drop(components);
        drop(ipc);
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
    endpoint_cleanup: EndpointCleanupV1,
    failures: Vec<String>,
) -> Result<DaemonShutdownReport, DaemonRuntimeError> {
    if failures.is_empty() {
        Ok(DaemonShutdownReport { endpoint_cleanup })
    } else {
        Err(DaemonRuntimeError { failures })
    }
}

fn collect_shutdown_failure<E>(
    failures: &mut Vec<String>,
    component: &'static str,
    result: Result<(), E>,
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
}

struct BlockingTaskState {
    accepting: bool,
    tasks: JoinSet<()>,
}

impl BlockingTaskOwner {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(BlockingTaskShared {
                state: Mutex::new(BlockingTaskState {
                    accepting: true,
                    tasks: JoinSet::new(),
                }),
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

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::oneshot;
    use unity_asset_search_index::{AssetLoadBudget, IndexPaths, SearchIndex};
    use unity_asset_search_local::{
        EndpointClaimV1, EndpointNamespaceV1, EndpointStoreError, PrivateRootsV1, ProjectLocatorV1,
        VerifiedLocalStreamV1, generate_daemon_instance_id,
    };
    use unity_asset_search_protocol::{
        BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2, DaemonInstanceId,
        FrameLimits, decode_validated_frame, encode_frame,
    };

    use super::{
        AdmissionGate, BlockingTaskError, BlockingTaskOwner, DaemonRuntime, DaemonRuntimeParts,
        SupervisorPanicStage,
    };
    use crate::coordinator::{
        ReindexCoordinatorConfig, ReindexCoordinatorRuntime, ReindexExecution,
    };
    use crate::ipc::{Dispatcher, OperationRegistryOwner};
    use crate::watcher::MaintenanceRuntime;

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

    async fn bootstrap_fixture_client(
        client: &mut VerifiedLocalStreamV1,
        descriptor: unity_asset_search_local::EndpointDescriptorV1,
    ) {
        let hello = BootstrapHelloV2::new(
            descriptor.project_id(),
            descriptor.daemon_instance_id(),
            vec![BUSINESS_PROTOCOL_REVISION],
        )
        .unwrap();
        client
            .write_all(&encode_frame(&hello, FrameLimits::bootstrap()).unwrap())
            .await
            .unwrap();

        let mut header = [0_u8; 4];
        client.read_exact(&mut header).await.unwrap();
        let length = u32::from_be_bytes(header) as usize;
        assert!(length <= FrameLimits::bootstrap().max_encoded_bytes());
        let mut frame = Vec::with_capacity(4 + length);
        frame.extend_from_slice(&header);
        frame.resize(4 + length, 0);
        client.read_exact(&mut frame[4..]).await.unwrap();
        client
            .verify_received_message_principal(descriptor.security_context_id())
            .unwrap();
        let mut budget = AssetLoadBudget::default();
        let reply: BootstrapReplyV2 =
            decode_validated_frame(&frame, &mut budget, FrameLimits::bootstrap()).unwrap();
        assert_eq!(reply.selected_revision(), Some(BUSINESS_PROTOCOL_REVISION));
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
            unity_asset_search_local::EndpointCleanupV1::Removed
        );
        fixture.finish();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_deadline_aborts_a_real_session_before_blocking_work_releases_authority() {
        let mut fixture = RuntimeFixture::new(generate_daemon_instance_id().unwrap());
        fixture.wait_for_publication().await;
        let descriptor = fixture.namespace().discover_endpoint().unwrap();
        let mut client = descriptor
            .connect_verified(fixture.namespace(), Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        bootstrap_fixture_client(&mut client, descriptor.descriptor()).await;
        client.write_all(&32_u32.to_be_bytes()).await.unwrap();
        client.write_all(b"{").await.unwrap();
        client.flush().await.unwrap();

        fixture.runtime().begin_shutdown(Duration::ZERO);
        let mut trailing = [0_u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(5), client.read(&mut trailing))
            .await
            .expect("drain deadline closes the partial-frame session");
        match closed {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::NotConnected
                ) || error.raw_os_error() == Some(233) => {}
            Ok(_) => panic!("draining session returned unexpected response bytes"),
            Err(error) => panic!("draining session failed unexpectedly: {error}"),
        }
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
            unity_asset_search_local::EndpointCleanupV1::Removed
        );
        drop(client);
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
    async fn service_loop_panic_joins_spawned_session_before_releasing_authority() {
        let mut fixture = RuntimeFixture::new_with_panic(
            generate_daemon_instance_id().unwrap(),
            SupervisorPanicStage::AfterSessionSpawn,
        );
        fixture.wait_for_publication().await;
        let descriptor = fixture.namespace().discover_endpoint().unwrap();
        let client = descriptor
            .connect_verified(fixture.namespace(), Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(10), fixture.runtime_mut().run())
                .await
                .is_err()
        );
        fixture.assert_authority_is_still_held();

        fixture.release_blocking_work();
        let error = fixture.runtime_mut().run().await.unwrap_err();
        assert!(error.to_string().contains("supervisor panicked"));
        drop(client);
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
            let endpoint_claim = namespace.claim_daemon_endpoint().unwrap();
            let index_paths = IndexPaths::for_project(
                locator.root().to_path_buf(),
                Some(project.path().join(".lifecycle-index")),
                None,
            )
            .unwrap();
            let mut budget = AssetLoadBudget::default();
            let index = SearchIndex::open_or_create(index_paths.clone(), &mut budget).unwrap();
            let query_policy_id = index.status().unwrap().query_policy_id;

            let project_root = locator.root().to_path_buf();
            let (started_sender, started_receiver) = mpsc::sync_channel(1);
            let (release_sender, release_receiver) = mpsc::channel();
            let runtime = start_fixture_runtime(
                endpoint_claim,
                daemon_instance_id,
                project_root,
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
                release: Some(release_sender),
            }
        }

        async fn wait_for_publication(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.namespace().discover_endpoint() {
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
                self.namespace().claim_daemon_endpoint(),
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
                match self.namespace().claim_daemon_endpoint() {
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
            let replacement = self.namespace().claim_daemon_endpoint().unwrap();
            drop(replacement);
            let mut budget = AssetLoadBudget::default();
            let reopened =
                SearchIndex::open_or_create(self.index_paths.clone(), &mut budget).unwrap();
            drop(reopened);
            drop(self.runtime.take());
            drop(self.namespace.take());
            for name in [
                "binding.v1",
                ".binding-v1.lock",
                ".daemon-v1.lock",
                "windows-pipe-slot.v1.json",
            ] {
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
        endpoint_claim: EndpointClaimV1,
        daemon_instance_id: DaemonInstanceId,
        project_root: PathBuf,
        query_policy_id: unity_asset_search_protocol::QueryPolicyId,
        index: SearchIndex,
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        panic_stage: Option<SupervisorPanicStage>,
    ) -> DaemonRuntime {
        DaemonRuntime::start_with_factory(move || -> anyhow::Result<_> {
            let blocking_tasks = BlockingTaskOwner::new();
            let blocking_handle = blocking_tasks.handle();
            drop(tokio::spawn(async move {
                let _ = blocking_handle
                    .run(move || {
                        let _ = started.send(());
                        release.recv().unwrap();
                    })
                    .await;
            }));

            let coordinator = ReindexCoordinatorRuntime::start(
                ReindexCoordinatorConfig::new(&project_root),
                |_| async {
                    Err::<ReindexExecution, _>(anyhow::anyhow!(
                        "lifecycle fixture must not execute a reindex"
                    ))
                },
            )?;
            let admission = AdmissionGate::default();
            let operations = OperationRegistryOwner::new(
                daemon_instance_id,
                coordinator.coordinator(),
                project_root,
                query_policy_id,
                admission.clone(),
            );
            let maintenance = MaintenanceRuntime::start(operations.registry(), None, None);
            let dispatcher = Dispatcher::new(
                index.clone(),
                blocking_tasks.handle(),
                operations.registry(),
                admission,
                maintenance.handle(),
            );
            Ok(DaemonRuntimeParts {
                endpoint_claim,
                daemon_instance_id,
                startup_reindex: None,
                dispatcher,
                maintenance,
                coordinator,
                operations,
                blocking_tasks,
                index,
                panic_stage,
            })
        })
        .unwrap()
    }
}
