//! Supervised filesystem watching and independent periodic reconciliation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use notify::Watcher as _;
use tokio::sync::{RwLock, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use unity_asset_search_index::{
    FilesystemReindexIntent, IndexPaths, ProjectPath, ProjectPathSet, ProjectPathSpace,
    is_search_ignore_v1_file_name,
};
use unity_asset_search_protocol::{ApiError, ProjectId};

use crate::coordinator::ReindexSource;
use crate::ipc::OperationRegistry;

const WATCH_CHANNEL_CAPACITY: usize = 1_024;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const MAXIMUM_RETRY_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub paths: IndexPaths,
}

impl WatcherConfig {
    #[must_use]
    pub(crate) fn project_id(&self) -> ProjectId {
        self.paths.project_id()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherLifecycle {
    Disabled,
    Starting,
    Healthy,
    Retrying,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerLifecycle {
    Disabled,
    Scheduled,
    Running,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceSnapshot {
    pub watcher: WatcherLifecycle,
    pub watcher_retry_count: u64,
    pub watcher_last_failure: Option<String>,
    pub watcher_next_retry_in_ms: Option<u64>,
    pub timer: TimerLifecycle,
    pub timer_run_count: u64,
    pub timer_last_failure: Option<String>,
    pub timer_next_run_in_ms: Option<u64>,
}

#[derive(Clone)]
pub struct MaintenanceHandle {
    state: Arc<RwLock<MaintenanceState>>,
}

#[must_use = "maintenance supervisors must be shut down and joined before coordinator shutdown"]
pub struct MaintenanceRuntime {
    handle: MaintenanceHandle,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<anyhow::Result<()>>,
}

struct MaintenanceState {
    watcher: WatcherLifecycle,
    watcher_retry_count: u64,
    watcher_last_failure: Option<String>,
    watcher_next_retry: Option<Instant>,
    timer: TimerLifecycle,
    timer_run_count: u64,
    timer_last_failure: Option<String>,
    timer_next_run: Option<Instant>,
}

impl MaintenanceRuntime {
    pub fn start(
        operations: OperationRegistry,
        watcher: Option<WatcherConfig>,
        reconcile_interval: Option<Duration>,
    ) -> Self {
        Self::start_with_dependencies(
            Arc::new(operations),
            watcher,
            reconcile_interval,
            Arc::new(NotifyWatcherFactory),
        )
    }

    fn start_with_dependencies(
        operations: Arc<dyn MaintenanceOperations>,
        watcher: Option<WatcherConfig>,
        reconcile_interval: Option<Duration>,
        watcher_factory: Arc<dyn WatcherFactory>,
    ) -> Self {
        let state = Arc::new(RwLock::new(MaintenanceState {
            watcher: watcher
                .as_ref()
                .map_or(WatcherLifecycle::Disabled, |_| WatcherLifecycle::Starting),
            watcher_retry_count: 0,
            watcher_last_failure: None,
            watcher_next_retry: None,
            timer: reconcile_interval
                .map_or(TimerLifecycle::Disabled, |_| TimerLifecycle::Scheduled),
            timer_run_count: 0,
            timer_last_failure: None,
            timer_next_run: reconcile_interval.map(|interval| Instant::now() + interval),
        }));
        let handle = MaintenanceHandle {
            state: Arc::clone(&state),
        };
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut tasks = JoinSet::new();
        if let Some(config) = watcher {
            tasks.spawn(supervise_watcher(
                operations.clone(),
                config,
                Arc::clone(&state),
                shutdown_receiver.clone(),
                watcher_factory,
            ));
        }
        if let Some(interval) = reconcile_interval {
            tasks.spawn(reconcile_loop(
                operations,
                interval,
                state,
                shutdown_receiver,
            ));
        }
        Self {
            handle,
            shutdown,
            tasks,
        }
    }

    #[must_use]
    pub fn handle(&self) -> MaintenanceHandle {
        self.handle.clone()
    }

    pub async fn shutdown(&mut self) -> Result<(), MaintenanceError> {
        self.shutdown.send_replace(true);
        let mut first_failure = None;
        while let Some(result) = self.tasks.join_next().await {
            let failure = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            if first_failure.is_none() {
                first_failure = failure;
            }
        }
        match first_failure {
            Some(message) => Err(MaintenanceError::SupervisorTerminated(message)),
            None => Ok(()),
        }
    }
}

impl MaintenanceHandle {
    pub async fn snapshot(&self) -> MaintenanceSnapshot {
        let state = self.state.read().await;
        let watcher_next_retry_in_ms = remaining_millis(state.watcher_next_retry);
        let timer_next_run_in_ms = state.timer_next_run.map(|next| {
            u64::try_from(next.saturating_duration_since(Instant::now()).as_millis())
                .unwrap_or(u64::MAX)
        });
        MaintenanceSnapshot {
            watcher: state.watcher,
            watcher_retry_count: state.watcher_retry_count,
            watcher_last_failure: state.watcher_last_failure.clone(),
            watcher_next_retry_in_ms,
            timer: state.timer,
            timer_run_count: state.timer_run_count,
            timer_last_failure: state.timer_last_failure.clone(),
            timer_next_run_in_ms,
        }
    }
}

fn remaining_millis(deadline: Option<Instant>) -> Option<u64> {
    deadline.map(|deadline| {
        u64::try_from(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(u64::MAX)
    })
}

type AdmissionFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>>;

trait MaintenanceOperations: Send + Sync {
    fn admit(&self, source: ReindexSource, intent: FilesystemReindexIntent) -> AdmissionFuture<'_>;

    fn admit_watcher_overflow(&self) -> AdmissionFuture<'_>;
}

impl MaintenanceOperations for OperationRegistry {
    fn admit(&self, source: ReindexSource, intent: FilesystemReindexIntent) -> AdmissionFuture<'_> {
        Box::pin(async move {
            OperationRegistry::admit_internal(self, source, intent).await?;
            Ok(())
        })
    }

    fn admit_watcher_overflow(&self) -> AdmissionFuture<'_> {
        Box::pin(async move {
            OperationRegistry::admit_watcher_overflow(self).await?;
            Ok(())
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MaintenanceError {
    #[error("maintenance supervisor terminated unexpectedly: {0}")]
    SupervisorTerminated(String),
}

type WatcherEventFuture<'a> = Pin<Box<dyn Future<Output = WatcherSessionEvent> + Send + 'a>>;

trait WatcherFactory: Send + Sync {
    fn open(&self, config: &WatcherConfig) -> anyhow::Result<Box<dyn WatcherSession>>;
}

trait WatcherSession: Send {
    fn next(&mut self) -> WatcherEventFuture<'_>;
}

struct NotifyWatcherFactory;

struct NotifyWatcherSession {
    _event_watcher: notify::RecommendedWatcher,
    _root_watcher: Option<notify::RecommendedWatcher>,
    stream: WatcherEventStream,
}

struct WatcherEventStream {
    events: mpsc::Receiver<WatchSignal>,
    failures: watch::Receiver<Option<String>>,
    overflowed: Arc<AtomicBool>,
}

enum WatcherSessionEvent {
    Changed(ProjectPathSet),
    Overflow,
    Failed(String),
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
enum WatchSignal {
    Changed(ProjectPathSet),
    Rescan,
}

impl WatcherFactory for NotifyWatcherFactory {
    fn open(&self, config: &WatcherConfig) -> anyhow::Result<Box<dyn WatcherSession>> {
        config.paths.revalidate_project_root()?;
        let (event_sender, event_receiver) = mpsc::channel(WATCH_CHANNEL_CAPACITY);
        let (failure_sender, failure_receiver) = watch::channel(None::<String>);
        let overflowed = Arc::new(AtomicBool::new(false));

        let callback_events = event_sender.clone();
        let callback_failures = failure_sender.clone();
        let callback_overflowed = Arc::clone(&overflowed);
        let project_paths = config.paths.project_path_space().clone();
        let event_project_paths = project_paths.clone();
        let index_namespace_exclusion = typed_index_namespace_exclusion(&config.paths)?;
        let mut event_watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                match event {
                    Ok(event) => send_watch_signal(
                        watch_signal(
                            event,
                            &event_project_paths,
                            index_namespace_exclusion.as_ref(),
                        ),
                        &callback_events,
                        &callback_overflowed,
                    ),
                    Err(error) => {
                        report_backend_failure(error.to_string(), &callback_failures);
                    }
                }
            })?;

        for root in config.paths.scan_roots() {
            event_watcher.watch(root, notify::RecursiveMode::Recursive)?;
        }

        let root_watcher = if config
            .paths
            .scan_roots()
            .iter()
            .any(|root| root == config.paths.project_root())
        {
            None
        } else {
            let root_events = event_sender.clone();
            let root_failures = failure_sender;
            let root_overflowed = Arc::clone(&overflowed);
            let root_project_paths = project_paths;
            let mut root_watcher =
                notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                    match event {
                        Ok(event) => send_watch_signal(
                            project_root_watch_signal(event, &root_project_paths),
                            &root_events,
                            &root_overflowed,
                        ),
                        Err(error) => {
                            report_backend_failure(error.to_string(), &root_failures);
                        }
                    }
                })?;
            root_watcher.watch(
                config.paths.project_root(),
                notify::RecursiveMode::NonRecursive,
            )?;
            Some(root_watcher)
        };
        config.paths.revalidate_project_root()?;
        drop(event_sender);

        Ok(Box::new(NotifyWatcherSession {
            _event_watcher: event_watcher,
            _root_watcher: root_watcher,
            stream: WatcherEventStream {
                events: event_receiver,
                failures: failure_receiver,
                overflowed,
            },
        }))
    }
}

fn typed_index_namespace_exclusion(paths: &IndexPaths) -> anyhow::Result<Option<ProjectPath>> {
    let Some(exclusion) = paths.index_namespace_exclusion() else {
        return Ok(None);
    };
    let Some(exclusion) = paths.project_path_space().resolve(exclusion)? else {
        anyhow::bail!("private index namespace exclusion resolved to the project root");
    };
    Ok(Some(exclusion))
}

impl WatcherSession for NotifyWatcherSession {
    fn next(&mut self) -> WatcherEventFuture<'_> {
        Box::pin(self.stream.next_event())
    }
}

impl WatcherEventStream {
    async fn next_event(&mut self) -> WatcherSessionEvent {
        loop {
            if let Some(failure) = self.take_failure() {
                return failure;
            }
            if self.take_overflow() {
                if let Some(failure) = self.take_failure() {
                    return failure;
                }
                return WatcherSessionEvent::Overflow;
            }

            enum Selected {
                Failure,
                Event(Option<WatchSignal>),
            }

            let selected = tokio::select! {
                biased;
                _ = self.failures.changed() => Selected::Failure,
                event = self.events.recv() => Selected::Event(event),
            };
            match selected {
                Selected::Failure => continue,
                Selected::Event(None) => {
                    return WatcherSessionEvent::Closed;
                }
                Selected::Event(Some(signal)) => {
                    if let Some(failure) = self.take_failure() {
                        return failure;
                    }
                    if self.take_overflow() {
                        return WatcherSessionEvent::Overflow;
                    }
                    return match signal {
                        WatchSignal::Changed(paths) => WatcherSessionEvent::Changed(paths),
                        WatchSignal::Rescan => WatcherSessionEvent::Overflow,
                    };
                }
            }
        }
    }

    fn take_failure(&mut self) -> Option<WatcherSessionEvent> {
        match self.failures.has_changed() {
            Ok(true) | Err(_) => Some(
                self.failures
                    .borrow_and_update()
                    .clone()
                    .map_or(WatcherSessionEvent::Closed, WatcherSessionEvent::Failed),
            ),
            Ok(false) => None,
        }
    }

    fn take_overflow(&mut self) -> bool {
        if !self.overflowed.swap(false, Ordering::AcqRel) {
            return false;
        }
        while self.events.try_recv().is_ok() {}
        true
    }
}

async fn supervise_watcher(
    operations: Arc<dyn MaintenanceOperations>,
    config: WatcherConfig,
    state: Arc<RwLock<MaintenanceState>>,
    mut shutdown: watch::Receiver<bool>,
    factory: Arc<dyn WatcherFactory>,
) -> anyhow::Result<()> {
    let mut backoff = INITIAL_RETRY_BACKOFF;
    loop {
        {
            let mut state = state.write().await;
            state.watcher = WatcherLifecycle::Starting;
            state.watcher_next_retry = None;
        }

        let failure = match factory.open(&config) {
            Ok(mut session) => {
                {
                    let mut state = state.write().await;
                    state.watcher = WatcherLifecycle::Healthy;
                    state.watcher_next_retry = None;
                }
                backoff = INITIAL_RETRY_BACKOFF;
                tokio::select! {
                    biased;
                    () = shutdown_requested(&mut shutdown) => {
                        stop_watcher(&state).await;
                        return Ok(());
                    }
                    failure = run_watcher_session(operations.as_ref(), session.as_mut()) => failure,
                }
            }
            Err(error) => bounded_failure(error.to_string()),
        };

        let retry_deadline = Instant::now() + backoff;
        {
            let mut state = state.write().await;
            state.watcher = WatcherLifecycle::Retrying;
            state.watcher_retry_count = state.watcher_retry_count.saturating_add(1);
            state.watcher_last_failure = Some(bounded_failure(failure));
            state.watcher_next_retry = Some(retry_deadline);
        }
        tokio::select! {
            biased;
            () = shutdown_requested(&mut shutdown) => {
                stop_watcher(&state).await;
                return Ok(());
            }
            () = tokio::time::sleep_until(retry_deadline) => {}
        }
        backoff = backoff
            .checked_mul(2)
            .unwrap_or(MAXIMUM_RETRY_BACKOFF)
            .min(MAXIMUM_RETRY_BACKOFF);
    }
}

async fn stop_watcher(state: &RwLock<MaintenanceState>) {
    let mut state = state.write().await;
    state.watcher = WatcherLifecycle::Stopped;
    state.watcher_next_retry = None;
}

async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    loop {
        let requested = *shutdown.borrow_and_update();
        if requested || shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn run_watcher_session(
    operations: &dyn MaintenanceOperations,
    session: &mut dyn WatcherSession,
) -> String {
    loop {
        match session.next().await {
            WatcherSessionEvent::Changed(paths) => {
                record_admission_failure(
                    operations
                        .admit(
                            ReindexSource::Watcher,
                            FilesystemReindexIntent::changed_paths(paths),
                        )
                        .await,
                    "watcher changed-path admission",
                );
            }
            WatcherSessionEvent::Overflow => {
                admit_watcher_overflow(operations).await;
            }
            WatcherSessionEvent::Failed(message) => return bounded_failure(message),
            WatcherSessionEvent::Closed => {
                return "filesystem watcher event channel closed unexpectedly".to_owned();
            }
        }
    }
}

fn send_watch_signal(
    signal: Option<WatchSignal>,
    sender: &mpsc::Sender<WatchSignal>,
    overflowed: &AtomicBool,
) {
    let Some(signal) = signal else {
        return;
    };
    if signal == WatchSignal::Rescan {
        overflowed.store(true, Ordering::Release);
    }
    match sender.try_send(signal) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            overflowed.store(true, Ordering::Release);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

fn report_backend_failure(message: String, failures: &watch::Sender<Option<String>>) {
    drop(failures.send_replace(Some(bounded_failure(message))));
}

async fn admit_watcher_overflow(operations: &dyn MaintenanceOperations) {
    record_admission_failure(
        operations.admit_watcher_overflow().await,
        "watcher overflow admission",
    );
}

fn record_admission_failure(result: Result<(), ApiError>, context: &str) {
    if let Err(error) = result
        && error.code != unity_asset_search_protocol::ApiErrorCode::NotReady
    {
        eprintln!("{context} failed: {error:?}");
    }
}

async fn reconcile_loop(
    operations: Arc<dyn MaintenanceOperations>,
    interval: Duration,
    state: Arc<RwLock<MaintenanceState>>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut deadline = Instant::now() + interval;
    loop {
        {
            let mut state = state.write().await;
            state.timer = TimerLifecycle::Scheduled;
            state.timer_next_run = Some(deadline);
        }
        tokio::select! {
            biased;
            () = shutdown_requested(&mut shutdown) => {
                let mut state = state.write().await;
                state.timer = TimerLifecycle::Stopped;
                state.timer_next_run = None;
                return Ok(());
            }
            () = tokio::time::sleep_until(deadline) => {}
        }

        {
            let mut state = state.write().await;
            state.timer = TimerLifecycle::Running;
            state.timer_next_run = None;
        }
        let admission =
            operations.admit(ReindexSource::Timer, FilesystemReindexIntent::reconcile());
        tokio::pin!(admission);
        let result = tokio::select! {
            biased;
            () = shutdown_requested(&mut shutdown) => {
                let mut state = state.write().await;
                state.timer = TimerLifecycle::Stopped;
                state.timer_next_run = None;
                return Ok(());
            }
            result = &mut admission => result,
        };
        deadline = Instant::now() + interval;
        let mut state = state.write().await;
        state.timer_run_count = state.timer_run_count.saturating_add(1);
        state.timer_last_failure = result
            .err()
            .map(|error| bounded_failure(format!("{error:?}")));
        state.timer = TimerLifecycle::Scheduled;
        state.timer_next_run = Some(deadline);
    }
}

fn watch_signal(
    event: notify::Event,
    project_paths: &ProjectPathSpace,
    index_namespace_exclusion: Option<&ProjectPath>,
) -> Option<WatchSignal> {
    if event.need_rescan() {
        return Some(WatchSignal::Rescan);
    }
    if index_namespace_exclusion.is_some_and(|root| root.project_id() != project_paths.project_id())
    {
        return Some(WatchSignal::Rescan);
    }
    let resolved = match project_paths.resolve_set(event.paths) {
        Ok(paths) => paths,
        Err(_) => return Some(WatchSignal::Rescan),
    };
    let mut changed = ProjectPathSet::new(project_paths);
    for path in resolved.into_paths() {
        if index_namespace_exclusion.is_some_and(|root| path.is_at_or_below(root)) {
            continue;
        }
        let Some(file_name) = path.file_name() else {
            return Some(WatchSignal::Rescan);
        };
        if is_legacy_ignore_file_name(file_name)
            || (is_search_ignore_v1_file_name(file_name)
                && !is_project_root_search_policy_path(&path))
        {
            continue;
        }
        if changed.insert(path).is_err() {
            return Some(WatchSignal::Rescan);
        }
    }
    (!changed.is_empty()).then_some(WatchSignal::Changed(changed))
}

fn project_root_watch_signal(
    event: notify::Event,
    project_paths: &ProjectPathSpace,
) -> Option<WatchSignal> {
    if event.need_rescan() {
        return Some(WatchSignal::Rescan);
    }
    let resolved = match project_paths.resolve_set(event.paths) {
        Ok(paths) => paths,
        Err(_) => return Some(WatchSignal::Rescan),
    };
    let mut changed = ProjectPathSet::new(project_paths);
    for path in resolved.into_paths() {
        if is_project_root_search_policy_path(&path) && changed.insert(path).is_err() {
            return Some(WatchSignal::Rescan);
        }
    }
    (!changed.is_empty()).then_some(WatchSignal::Changed(changed))
}

fn is_project_root_search_policy_path(path: &ProjectPath) -> bool {
    path.as_relative_path().components().count() == 1
        && path.file_name().is_some_and(is_search_ignore_v1_file_name)
}

#[cfg(not(windows))]
fn is_legacy_ignore_file_name(name: &std::ffi::OsStr) -> bool {
    name == ".gitignore" || name == ".ignore"
}

#[cfg(windows)]
fn is_legacy_ignore_file_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.eq_ignore_ascii_case(".gitignore") || name.eq_ignore_ascii_case(".ignore")
    })
}

fn bounded_failure(mut message: String) -> String {
    const MAXIMUM: usize = 4 * 1024;
    if message.trim().is_empty() {
        return "maintenance task failed without diagnostic evidence".to_owned();
    }
    if message.len() <= MAXIMUM {
        return message;
    }
    let mut boundary = MAXIMUM;
    while !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message.truncate(boundary);
    message
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::{mpsc, watch};
    use unity_asset_search_index::{
        FilesystemReindexIntent, FilesystemReindexScope, IndexPaths, ProjectPath, ProjectPathSet,
    };
    use unity_asset_search_protocol::{ApiError, ApiErrorCode};

    use super::{
        AdmissionFuture, MaintenanceOperations, MaintenanceRuntime, NotifyWatcherFactory,
        TimerLifecycle, WatchSignal, WatcherConfig, WatcherEventFuture, WatcherEventStream,
        WatcherFactory, WatcherLifecycle, WatcherSession, WatcherSessionEvent,
        is_project_root_search_policy_path, project_root_watch_signal, report_backend_failure,
        run_watcher_session, send_watch_signal, typed_index_namespace_exclusion, watch_signal,
    };
    use crate::coordinator::ReindexSource;

    #[derive(Default)]
    struct ScriptedOperations {
        timer_results: Mutex<VecDeque<Result<(), ApiError>>>,
        timer_calls: AtomicUsize,
        watcher_calls: AtomicUsize,
        admissions: Mutex<Vec<ScriptedAdmission>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ScriptedAdmission {
        Reindex {
            source: ReindexSource,
            intent: FilesystemReindexIntent,
        },
        WatcherOverflow,
    }

    impl ScriptedOperations {
        fn with_timer_results(results: impl IntoIterator<Item = Result<(), ApiError>>) -> Self {
            Self {
                timer_results: Mutex::new(results.into_iter().collect()),
                ..Self::default()
            }
        }

        fn admissions(&self) -> Vec<ScriptedAdmission> {
            self.admissions
                .lock()
                .expect("admission record should not be poisoned")
                .clone()
        }
    }

    impl MaintenanceOperations for ScriptedOperations {
        fn admit(
            &self,
            source: ReindexSource,
            intent: FilesystemReindexIntent,
        ) -> AdmissionFuture<'_> {
            self.admissions
                .lock()
                .expect("admission record should not be poisoned")
                .push(ScriptedAdmission::Reindex { source, intent });
            let result = if source == ReindexSource::Timer {
                self.timer_calls.fetch_add(1, Ordering::Relaxed);
                self.timer_results
                    .lock()
                    .expect("timer result script should not be poisoned")
                    .pop_front()
                    .unwrap_or(Ok(()))
            } else {
                self.watcher_calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            };
            Box::pin(std::future::ready(result))
        }

        fn admit_watcher_overflow(&self) -> AdmissionFuture<'_> {
            self.watcher_calls.fetch_add(1, Ordering::Relaxed);
            self.admissions
                .lock()
                .expect("admission record should not be poisoned")
                .push(ScriptedAdmission::WatcherOverflow);
            Box::pin(std::future::ready(Ok(())))
        }
    }

    enum ScriptedOpen {
        Fail(String),
        Session(mpsc::UnboundedReceiver<WatcherSessionEvent>),
    }

    struct ScriptedWatcherFactory {
        opens: Mutex<VecDeque<ScriptedOpen>>,
        open_count: AtomicUsize,
    }

    impl ScriptedWatcherFactory {
        fn new(opens: impl IntoIterator<Item = ScriptedOpen>) -> Self {
            Self {
                opens: Mutex::new(opens.into_iter().collect()),
                open_count: AtomicUsize::new(0),
            }
        }
    }

    impl WatcherFactory for ScriptedWatcherFactory {
        fn open(&self, _config: &WatcherConfig) -> anyhow::Result<Box<dyn WatcherSession>> {
            self.open_count.fetch_add(1, Ordering::Relaxed);
            match self
                .opens
                .lock()
                .expect("watcher open script should not be poisoned")
                .pop_front()
            {
                Some(ScriptedOpen::Fail(message)) => anyhow::bail!(message),
                Some(ScriptedOpen::Session(events)) => {
                    Ok(Box::new(ScriptedWatcherSession { events }))
                }
                None => anyhow::bail!("watcher open script exhausted"),
            }
        }
    }

    struct ScriptedWatcherSession {
        events: mpsc::UnboundedReceiver<WatcherSessionEvent>,
    }

    impl WatcherSession for ScriptedWatcherSession {
        fn next(&mut self) -> WatcherEventFuture<'_> {
            Box::pin(async move {
                self.events
                    .recv()
                    .await
                    .unwrap_or(WatcherSessionEvent::Closed)
            })
        }
    }

    struct WatcherFixture {
        config: WatcherConfig,
        project_input: PathBuf,
        _temporary: tempfile::TempDir,
    }

    impl WatcherFixture {
        fn new() -> Self {
            let temporary = crate::secure_test_tempdir();
            let project_input = temporary.path().join("Project");
            let index_base = project_input.join("Assets").join("SearchIndex");
            Self::create(temporary, project_input, index_base)
        }

        fn with_ancestor_index() -> Self {
            let temporary = crate::secure_test_tempdir();
            let index_base = temporary.path().join("IndexAndProjects");
            let bootstrap = temporary.path().join("Bootstrap");
            let bootstrap_assets = bootstrap.join("Assets");
            std::fs::create_dir_all(&bootstrap_assets)
                .expect("bootstrap scan root should be created");
            IndexPaths::for_project(
                bootstrap,
                Some(index_base.clone()),
                Some(vec![bootstrap_assets]),
            )
            .expect("ancestor index namespace should be initialized");
            let project_input = index_base.join("Project");
            Self::create(temporary, project_input, index_base)
        }

        fn create(
            temporary: tempfile::TempDir,
            project_input: PathBuf,
            index_base: PathBuf,
        ) -> Self {
            let assets = project_input.join("Assets");
            std::fs::create_dir_all(&assets).expect("project scan root should be created");
            let paths = IndexPaths::for_project(
                project_input.clone(),
                Some(index_base),
                Some(vec![assets]),
            )
            .expect("watcher fixture paths should be valid");
            Self {
                config: WatcherConfig { paths },
                project_input,
                _temporary: temporary,
            }
        }

        fn event_path(&self, relative: impl AsRef<Path>) -> PathBuf {
            self.project_input.join(relative)
        }

        fn project_path(&self, relative: impl AsRef<Path>) -> ProjectPath {
            self.config
                .paths
                .project_path_space()
                .resolve(relative.as_ref())
                .expect("fixture path should resolve")
                .expect("fixture path should not denote the project root")
        }

        fn changed_paths<I, P>(&self, paths: I) -> ProjectPathSet
        where
            I: IntoIterator<Item = P>,
            P: AsRef<Path>,
        {
            self.config
                .paths
                .project_path_space()
                .resolve_set(paths)
                .expect("fixture changed paths should resolve")
        }

        fn index_namespace_exclusion(&self) -> Option<ProjectPath> {
            typed_index_namespace_exclusion(&self.config.paths)
                .expect("fixture index namespace should resolve")
        }
    }

    fn scripted_session() -> (mpsc::UnboundedSender<WatcherSessionEvent>, ScriptedOpen) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (sender, ScriptedOpen::Session(receiver))
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn only_the_project_root_search_policy_is_watched_outside_scan_roots() {
        let fixture = WatcherFixture::new();

        assert!(!is_project_root_search_policy_path(
            &fixture.project_path(".gitignore")
        ));
        assert!(!is_project_root_search_policy_path(
            &fixture.project_path(".ignore")
        ));
        assert!(is_project_root_search_policy_path(
            &fixture.project_path(".unity-asset-search-ignore")
        ));
        #[cfg(any(windows, target_os = "macos"))]
        assert!(is_project_root_search_policy_path(
            &fixture.project_path(".UNITY-ASSET-SEARCH-IGNORE")
        ));
        #[cfg(not(any(windows, target_os = "macos")))]
        assert!(!is_project_root_search_policy_path(
            &fixture.project_path(".UNITY-ASSET-SEARCH-IGNORE")
        ));
        assert!(!is_project_root_search_policy_path(
            &fixture.project_path("Assets/.unity-asset-search-ignore")
        ));
        assert!(!is_project_root_search_policy_path(
            &fixture.project_path("README.md")
        ));
    }

    #[test]
    fn project_root_watcher_forwards_only_the_root_search_policy() {
        let fixture = WatcherFixture::new();
        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![
                fixture.event_path(".unity-asset-search-ignore"),
                fixture.event_path(".gitignore"),
                fixture.event_path("Assets/.unity-asset-search-ignore"),
                fixture.event_path("README.md"),
            ],
            attrs: notify::event::EventAttributes::new(),
        };

        let signal = project_root_watch_signal(event, fixture.config.paths.project_path_space())
            .expect("root policy should be forwarded");
        let WatchSignal::Changed(paths) = signal else {
            panic!("root policy should remain an incremental signal")
        };
        assert_eq!(paths, fixture.changed_paths([".unity-asset-search-ignore"]));
    }

    #[test]
    fn scan_root_events_filter_the_complete_index_namespace_without_policy_aliases() {
        let fixture = WatcherFixture::new();
        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![
                fixture.event_path("Assets/Foo.prefab"),
                fixture.event_path("Assets/.gitignore"),
                fixture.event_path("Assets/.ignore"),
                fixture.event_path("Assets/.unity-asset-search-ignore"),
                fixture.event_path(".unity-asset-search-ignore"),
                fixture.event_path("Assets/SearchIndex/index-v1-a/generation.bin"),
                fixture.event_path("Assets/SearchIndex/index-v1-b/generation.bin"),
            ],
            attrs: notify::event::EventAttributes::new(),
        };
        let namespace = fixture
            .index_namespace_exclusion()
            .expect("nested fixture index namespace should be excludable");

        let signal = watch_signal(
            event,
            fixture.config.paths.project_path_space(),
            Some(&namespace),
        )
        .expect("source paths must be forwarded");
        let WatchSignal::Changed(paths) = signal else {
            panic!("ordinary source paths must remain an incremental signal")
        };
        assert_eq!(
            paths,
            fixture.changed_paths(["Assets/Foo.prefab", ".unity-asset-search-ignore"])
        );
        assert!(!is_project_root_search_policy_path(
            &fixture.project_path(".gitignore")
        ));
    }

    #[test]
    fn an_ancestor_index_namespace_never_filters_project_events() {
        let fixture = WatcherFixture::with_ancestor_index();
        assert_eq!(fixture.index_namespace_exclusion(), None);
        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![fixture.event_path("Assets/Foo.prefab")],
            attrs: notify::event::EventAttributes::new(),
        };

        let signal = watch_signal(event, fixture.config.paths.project_path_space(), None)
            .expect("an ancestor namespace must not suppress project events");
        let WatchSignal::Changed(paths) = signal else {
            panic!("ordinary source paths must remain an incremental signal")
        };

        assert_eq!(paths, fixture.changed_paths(["Assets/Foo.prefab"]));
    }

    #[tokio::test]
    async fn backend_rescan_without_paths_escalates_to_overflow_for_both_watchers() {
        let fixture = WatcherFixture::new();
        let event =
            notify::Event::new(notify::EventKind::Any).set_flag(notify::event::Flag::Rescan);
        assert_eq!(
            watch_signal(
                event.clone(),
                fixture.config.paths.project_path_space(),
                None,
            ),
            Some(WatchSignal::Rescan)
        );
        assert_eq!(
            project_root_watch_signal(event, fixture.config.paths.project_path_space()),
            Some(WatchSignal::Rescan)
        );

        let (event_sender, event_receiver) = mpsc::channel(1);
        let (_failure_sender, failure_receiver) = watch::channel(None::<String>);
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        send_watch_signal(
            Some(WatchSignal::Rescan),
            &event_sender,
            overflowed.as_ref(),
        );
        let mut stream = WatcherEventStream {
            events: event_receiver,
            failures: failure_receiver,
            overflowed,
        };

        assert!(matches!(
            stream.next_event().await,
            WatcherSessionEvent::Overflow
        ));
    }

    #[test]
    fn any_event_path_conversion_failure_escalates_the_whole_batch_to_rescan() {
        let fixture = WatcherFixture::new();
        let outside = fixture
            .project_input
            .parent()
            .expect("fixture project should have a parent")
            .join("Outside.asset");
        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![fixture.event_path("Assets/Foo.prefab"), outside],
            attrs: notify::event::EventAttributes::new(),
        };

        assert_eq!(
            watch_signal(
                event.clone(),
                fixture.config.paths.project_path_space(),
                fixture.index_namespace_exclusion().as_ref(),
            ),
            Some(WatchSignal::Rescan)
        );
        assert_eq!(
            project_root_watch_signal(event, fixture.config.paths.project_path_space()),
            Some(WatchSignal::Rescan)
        );

        let project_root_event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![fixture.project_input.clone()],
            attrs: notify::event::EventAttributes::new(),
        };
        assert_eq!(
            watch_signal(
                project_root_event,
                fixture.config.paths.project_path_space(),
                None,
            ),
            Some(WatchSignal::Rescan)
        );
    }

    #[test]
    fn a_foreign_project_namespace_exclusion_escalates_to_rescan() {
        let fixture = WatcherFixture::new();
        let foreign = WatcherFixture::new();
        let foreign_namespace = foreign
            .index_namespace_exclusion()
            .expect("foreign fixture should have a nested namespace");
        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![fixture.event_path("Assets/Foo.prefab")],
            attrs: notify::event::EventAttributes::new(),
        };

        assert_eq!(
            watch_signal(
                event,
                fixture.config.paths.project_path_space(),
                Some(&foreign_namespace),
            ),
            Some(WatchSignal::Rescan)
        );
    }

    #[cfg(windows)]
    #[test]
    fn watcher_delegates_drive_verbatim_and_case_aliases_to_project_path_space() {
        let fixture = WatcherFixture::new();
        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![
                fixture.event_path("assets/HERO.prefab"),
                fixture
                    .config
                    .paths
                    .project_root()
                    .join("Assets/Hero.prefab"),
            ],
            attrs: notify::event::EventAttributes::new(),
        };

        let signal = watch_signal(event, fixture.config.paths.project_path_space(), None)
            .expect("Windows aliases should remain incremental");
        let WatchSignal::Changed(paths) = signal else {
            panic!("Windows aliases should resolve to typed changed paths")
        };
        assert_eq!(paths.len(), 1);
        assert_eq!(paths, fixture.changed_paths(["Assets/Hero.prefab"]));
    }

    #[tokio::test]
    async fn watcher_session_preserves_changed_paths_and_escalates_overflow() {
        let fixture = WatcherFixture::new();
        let operations = ScriptedOperations::default();
        let (sender, scripted) = scripted_session();
        let ScriptedOpen::Session(events) = scripted else {
            unreachable!("scripted_session must return a session")
        };
        let mut session = ScriptedWatcherSession { events };
        let changed = fixture.changed_paths([".unity-asset-search-ignore", "Assets/Foo.prefab"]);
        sender
            .send(WatcherSessionEvent::Changed(changed.clone()))
            .unwrap();
        sender.send(WatcherSessionEvent::Overflow).unwrap();
        sender.send(WatcherSessionEvent::Closed).unwrap();

        let failure = run_watcher_session(&operations, &mut session).await;

        assert_eq!(
            failure,
            "filesystem watcher event channel closed unexpectedly"
        );
        assert_eq!(
            operations.admissions(),
            vec![
                ScriptedAdmission::Reindex {
                    source: ReindexSource::Watcher,
                    intent: FilesystemReindexIntent {
                        scope: FilesystemReindexScope::ChangedPaths { paths: changed },
                    },
                },
                ScriptedAdmission::WatcherOverflow,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backend_failure_is_not_hidden_by_a_saturated_event_queue() {
        let fixture = WatcherFixture::new();
        let (event_sender, event_receiver) = mpsc::channel(1);
        let (failure_sender, failure_receiver) = watch::channel(None::<String>);
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        send_watch_signal(
            Some(WatchSignal::Changed(
                fixture.changed_paths(["Assets/A.asset"]),
            )),
            &event_sender,
            overflowed.as_ref(),
        );
        send_watch_signal(
            Some(WatchSignal::Changed(
                fixture.changed_paths(["Assets/B.asset"]),
            )),
            &event_sender,
            overflowed.as_ref(),
        );
        report_backend_failure("backend disconnected".to_owned(), &failure_sender);
        drop(failure_sender);
        let mut stream = WatcherEventStream {
            events: event_receiver,
            failures: failure_receiver,
            overflowed,
        };

        assert!(matches!(
            stream.next_event().await,
            WatcherSessionEvent::Failed(message) if message == "backend disconnected"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_retries_with_backoff_and_resets_after_becoming_healthy() {
        let fixture = WatcherFixture::new();
        let (session_sender, session) = scripted_session();
        let factory = Arc::new(ScriptedWatcherFactory::new([
            ScriptedOpen::Fail("first initialization failure".to_owned()),
            ScriptedOpen::Fail("second initialization failure".to_owned()),
            session,
        ]));
        let operations = Arc::new(ScriptedOperations::default());
        let mut maintenance = MaintenanceRuntime::start_with_dependencies(
            operations,
            Some(fixture.config.clone()),
            None,
            factory.clone(),
        );
        let handle = maintenance.handle();
        settle().await;

        let first_retry = handle.snapshot().await;
        assert_eq!(first_retry.watcher, WatcherLifecycle::Retrying);
        assert_eq!(first_retry.watcher_retry_count, 1);
        assert_eq!(first_retry.watcher_next_retry_in_ms, Some(250));
        assert_eq!(factory.open_count.load(Ordering::Relaxed), 1);

        tokio::time::advance(Duration::from_millis(249)).await;
        settle().await;
        assert_eq!(factory.open_count.load(Ordering::Relaxed), 1);

        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        let second_retry = handle.snapshot().await;
        assert_eq!(second_retry.watcher, WatcherLifecycle::Retrying);
        assert_eq!(second_retry.watcher_retry_count, 2);
        assert_eq!(second_retry.watcher_next_retry_in_ms, Some(500));

        tokio::time::advance(Duration::from_millis(500)).await;
        settle().await;
        assert_eq!(handle.snapshot().await.watcher, WatcherLifecycle::Healthy);
        assert_eq!(factory.open_count.load(Ordering::Relaxed), 3);

        session_sender
            .send(WatcherSessionEvent::Failed("é".repeat(3_000)))
            .unwrap();
        settle().await;
        let runtime_retry = handle.snapshot().await;
        assert_eq!(runtime_retry.watcher, WatcherLifecycle::Retrying);
        assert_eq!(runtime_retry.watcher_retry_count, 3);
        assert_eq!(runtime_retry.watcher_next_retry_in_ms, Some(250));
        assert!(
            runtime_retry
                .watcher_last_failure
                .as_ref()
                .is_some_and(|failure| failure.len() <= 4 * 1024)
        );

        maintenance.shutdown().await.unwrap();

        let stopped = handle.snapshot().await;
        assert_eq!(stopped.watcher, WatcherLifecycle::Stopped);
        assert_eq!(stopped.watcher_next_retry_in_ms, None);
    }

    #[tokio::test(start_paused = true)]
    async fn timer_failure_remains_scheduled_and_success_clears_evidence() {
        let operations = Arc::new(ScriptedOperations::with_timer_results([
            Err(ApiError::new(ApiErrorCode::Busy, "busy", true)),
            Ok(()),
        ]));
        let mut maintenance = MaintenanceRuntime::start_with_dependencies(
            operations,
            None,
            Some(Duration::from_secs(10)),
            Arc::new(NotifyWatcherFactory),
        );
        let handle = maintenance.handle();
        settle().await;

        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        let failed_run = handle.snapshot().await;
        assert_eq!(failed_run.timer, TimerLifecycle::Scheduled);
        assert_eq!(failed_run.timer_run_count, 1);
        assert!(failed_run.timer_last_failure.is_some());
        assert_eq!(failed_run.timer_next_run_in_ms, Some(10_000));

        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        let successful_run = handle.snapshot().await;
        assert_eq!(successful_run.timer, TimerLifecycle::Scheduled);
        assert_eq!(successful_run.timer_run_count, 2);
        assert_eq!(successful_run.timer_last_failure, None);

        maintenance.shutdown().await.unwrap();
        assert_eq!(handle.snapshot().await.timer, TimerLifecycle::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_retry_does_not_block_timer_and_shutdown_joins_both() {
        let fixture = WatcherFixture::new();
        let factory = Arc::new(ScriptedWatcherFactory::new([ScriptedOpen::Fail(
            "watcher unavailable".to_owned(),
        )]));
        let operations = Arc::new(ScriptedOperations::default());
        let mut maintenance = MaintenanceRuntime::start_with_dependencies(
            operations.clone(),
            Some(fixture.config.clone()),
            Some(Duration::from_millis(100)),
            factory,
        );
        let handle = maintenance.handle();
        settle().await;
        assert_eq!(handle.snapshot().await.watcher, WatcherLifecycle::Retrying);

        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
        let snapshot = handle.snapshot().await;
        assert_eq!(snapshot.watcher, WatcherLifecycle::Retrying);
        assert_eq!(snapshot.timer_run_count, 1);
        assert_eq!(operations.timer_calls.load(Ordering::Relaxed), 1);

        maintenance.shutdown().await.unwrap();
        let stopped = handle.snapshot().await;
        assert_eq!(stopped.watcher, WatcherLifecycle::Stopped);
        assert_eq!(stopped.timer, TimerLifecycle::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_an_active_session_and_a_scheduled_timer() {
        let fixture = WatcherFixture::new();
        let (_session_sender, session) = scripted_session();
        let factory = Arc::new(ScriptedWatcherFactory::new([session]));
        let mut maintenance = MaintenanceRuntime::start_with_dependencies(
            Arc::new(ScriptedOperations::default()),
            Some(fixture.config.clone()),
            Some(Duration::from_secs(60 * 60)),
            factory,
        );
        let handle = maintenance.handle();
        settle().await;
        let running = handle.snapshot().await;
        assert_eq!(running.watcher, WatcherLifecycle::Healthy);
        assert_eq!(running.timer, TimerLifecycle::Scheduled);

        maintenance.shutdown().await.unwrap();

        let stopped = handle.snapshot().await;
        assert_eq!(stopped.watcher, WatcherLifecycle::Stopped);
        assert_eq!(stopped.timer, TimerLifecycle::Stopped);
    }
}
