//! Supervised filesystem watching and independent periodic reconciliation.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use notify::Watcher as _;
use tokio::sync::{RwLock, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use unity_asset_search_index::{FilesystemReindexIntent, is_search_ignore_v1_file_name};
use unity_asset_search_protocol::ApiError;

use crate::coordinator::ReindexSource;
use crate::ipc::OperationRegistry;

const WATCH_CHANNEL_CAPACITY: usize = 1_024;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const MAXIMUM_RETRY_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub scan_roots: Vec<PathBuf>,
    pub project_root: PathBuf,
    pub index_namespace_exclusion: Option<PathBuf>,
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
    Changed(Vec<PathBuf>),
    Overflow,
    Failed(String),
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
enum WatchSignal {
    Changed(Vec<PathBuf>),
    Rescan,
}

impl WatcherFactory for NotifyWatcherFactory {
    fn open(&self, config: &WatcherConfig) -> anyhow::Result<Box<dyn WatcherSession>> {
        let (event_sender, event_receiver) = mpsc::channel(WATCH_CHANNEL_CAPACITY);
        let (failure_sender, failure_receiver) = watch::channel(None::<String>);
        let overflowed = Arc::new(AtomicBool::new(false));

        let callback_events = event_sender.clone();
        let callback_failures = failure_sender.clone();
        let callback_overflowed = Arc::clone(&overflowed);
        let event_project_root = config.project_root.clone();
        let index_namespace_exclusion = config.index_namespace_exclusion.clone();
        let mut event_watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                match event {
                    Ok(event) => send_watch_signal(
                        watch_signal(
                            event,
                            &event_project_root,
                            index_namespace_exclusion.as_deref(),
                        ),
                        &callback_events,
                        &callback_overflowed,
                    ),
                    Err(error) => {
                        report_backend_failure(error.to_string(), &callback_failures);
                    }
                }
            })?;

        for root in &config.scan_roots {
            event_watcher.watch(root, notify::RecursiveMode::Recursive)?;
        }

        let root_watcher = if config
            .scan_roots
            .iter()
            .any(|root| root == &config.project_root)
        {
            None
        } else {
            let root_events = event_sender.clone();
            let root_failures = failure_sender;
            let root_overflowed = Arc::clone(&overflowed);
            let watched_project_root = config.project_root.clone();
            let mut root_watcher =
                notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                    match event {
                        Ok(event) => send_watch_signal(
                            project_root_watch_signal(event, &watched_project_root),
                            &root_events,
                            &root_overflowed,
                        ),
                        Err(error) => {
                            report_backend_failure(error.to_string(), &root_failures);
                        }
                    }
                })?;
            root_watcher.watch(&config.project_root, notify::RecursiveMode::NonRecursive)?;
            Some(root_watcher)
        };
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
    project_root: &Path,
    index_namespace_exclusion: Option<&Path>,
) -> Option<WatchSignal> {
    if event.need_rescan() {
        return Some(WatchSignal::Rescan);
    }
    let paths = event
        .paths
        .into_iter()
        .filter(|path| {
            !index_namespace_exclusion.is_some_and(|root| platform_path_starts_with(path, root))
        })
        .filter(|path| {
            path.file_name().is_none_or(|name| {
                !is_legacy_ignore_file_name(name)
                    && (!is_search_ignore_v1_file_name(name)
                        || is_project_root_search_policy_path(project_root, path))
            })
        })
        .collect::<Vec<_>>();
    (!paths.is_empty()).then_some(WatchSignal::Changed(paths))
}

fn project_root_watch_signal(event: notify::Event, project_root: &Path) -> Option<WatchSignal> {
    if event.need_rescan() {
        return Some(WatchSignal::Rescan);
    }
    let changed = event
        .paths
        .into_iter()
        .filter(|path| is_project_root_search_policy_path(project_root, path))
        .collect::<Vec<_>>();
    (!changed.is_empty()).then_some(WatchSignal::Changed(changed))
}

fn is_project_root_search_policy_path(project_root: &Path, path: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| platform_paths_equal(parent, project_root))
        && path.file_name().is_some_and(is_search_ignore_v1_file_name)
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn platform_path_starts_with(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn platform_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(target_os = "macos")]
fn platform_path_starts_with(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
        || (path.is_absolute()
            && prefix.is_absolute()
            && canonicalize_with_missing_tail(path)
                .is_some_and(|canonical| canonical.starts_with(prefix)))
}

#[cfg(target_os = "macos")]
fn platform_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
        || (left.is_absolute()
            && right.is_absolute()
            && canonicalize_with_missing_tail(left)
                .zip(canonicalize_with_missing_tail(right))
                .is_some_and(|(left, right)| left == right))
}

#[cfg(target_os = "macos")]
fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Some(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(existing.file_name()?.to_os_string());
                existing = existing.parent()?;
            }
            Err(_) => return None,
        }
    }
}

#[cfg(windows)]
fn platform_path_starts_with(path: &Path, prefix: &Path) -> bool {
    let mut components = path.components();
    prefix.components().all(|expected| {
        components
            .next()
            .is_some_and(|actual| windows_path_component_eq(actual, expected))
    })
}

#[cfg(windows)]
fn platform_paths_equal(left: &Path, right: &Path) -> bool {
    left.components().count() == right.components().count()
        && platform_path_starts_with(left, right)
}

#[cfg(windows)]
fn windows_path_component_eq(
    left: std::path::Component<'_>,
    right: std::path::Component<'_>,
) -> bool {
    use std::path::Component;

    match (left, right) {
        (Component::Prefix(left), Component::Prefix(right)) => {
            windows_prefix_eq(left.kind(), right.kind())
        }
        (Component::RootDir, Component::RootDir)
        | (Component::CurDir, Component::CurDir)
        | (Component::ParentDir, Component::ParentDir) => true,
        (Component::Normal(left), Component::Normal(right)) => windows_os_str_eq(left, right),
        _ => false,
    }
}

#[cfg(windows)]
fn windows_prefix_eq(left: std::path::Prefix<'_>, right: std::path::Prefix<'_>) -> bool {
    use std::path::Prefix;

    match (left, right) {
        (Prefix::Disk(left), Prefix::Disk(right))
        | (Prefix::Disk(left), Prefix::VerbatimDisk(right))
        | (Prefix::VerbatimDisk(left), Prefix::Disk(right))
        | (Prefix::VerbatimDisk(left), Prefix::VerbatimDisk(right)) => {
            left.eq_ignore_ascii_case(&right)
        }
        (Prefix::UNC(left_server, left_share), Prefix::UNC(right_server, right_share))
        | (Prefix::UNC(left_server, left_share), Prefix::VerbatimUNC(right_server, right_share))
        | (Prefix::VerbatimUNC(left_server, left_share), Prefix::UNC(right_server, right_share))
        | (
            Prefix::VerbatimUNC(left_server, left_share),
            Prefix::VerbatimUNC(right_server, right_share),
        ) => {
            windows_os_str_eq(left_server, right_server)
                && windows_os_str_eq(left_share, right_share)
        }
        _ => false,
    }
}

#[cfg(windows)]
fn windows_os_str_eq(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

    let left = left.encode_wide().collect::<Vec<_>>();
    let right = right.encode_wide().collect::<Vec<_>>();
    let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
    else {
        return false;
    };
    // SAFETY: both encoded buffers remain live for their exact lengths during the call.
    unsafe {
        CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) == CSTR_EQUAL
    }
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
    use unity_asset_search_index::{FilesystemReindexIntent, FilesystemReindexScope};
    use unity_asset_search_protocol::{ApiError, ApiErrorCode};

    use super::{
        AdmissionFuture, MaintenanceOperations, MaintenanceRuntime, NotifyWatcherFactory,
        TimerLifecycle, WatchSignal, WatcherConfig, WatcherEventFuture, WatcherEventStream,
        WatcherFactory, WatcherLifecycle, WatcherSession, WatcherSessionEvent,
        is_project_root_search_policy_path, project_root_watch_signal, report_backend_failure,
        run_watcher_session, send_watch_signal, watch_signal,
    };
    #[cfg(any(windows, target_os = "macos"))]
    use super::{platform_path_starts_with, platform_paths_equal};
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

    fn watcher_config() -> WatcherConfig {
        WatcherConfig {
            scan_roots: vec![PathBuf::from("project/Assets")],
            project_root: PathBuf::from("project"),
            index_namespace_exclusion: Some(PathBuf::from("index")),
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
        let root = Path::new("project");

        assert!(!is_project_root_search_policy_path(
            root,
            Path::new("project/.gitignore")
        ));
        assert!(!is_project_root_search_policy_path(
            root,
            Path::new("project/.ignore")
        ));
        assert!(is_project_root_search_policy_path(
            root,
            Path::new("project/.unity-asset-search-ignore")
        ));
        #[cfg(any(windows, target_os = "macos"))]
        assert!(is_project_root_search_policy_path(
            root,
            Path::new("project/.UNITY-ASSET-SEARCH-IGNORE")
        ));
        #[cfg(not(any(windows, target_os = "macos")))]
        assert!(!is_project_root_search_policy_path(
            root,
            Path::new("project/.UNITY-ASSET-SEARCH-IGNORE")
        ));
        assert!(!is_project_root_search_policy_path(
            root,
            Path::new("project/Assets/.unity-asset-search-ignore")
        ));
        assert!(!is_project_root_search_policy_path(
            root,
            Path::new("project/README.md")
        ));
    }

    #[test]
    fn scan_root_events_filter_the_complete_index_namespace_without_policy_aliases() {
        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![
                PathBuf::from("project/Assets/Foo.prefab"),
                PathBuf::from("project/Assets/.gitignore"),
                PathBuf::from("project/Assets/.unity-asset-search-ignore"),
                PathBuf::from("project/.unity-asset-search-ignore"),
                PathBuf::from("project/Assets/SearchIndex/index-v1-a/generation.bin"),
                PathBuf::from("project/Assets/SearchIndex/index-v1-b/generation.bin"),
            ],
            attrs: notify::event::EventAttributes::new(),
        };

        let signal = watch_signal(
            event,
            Path::new("project"),
            Some(Path::new("project/Assets/SearchIndex")),
        )
        .expect("source paths must be forwarded");
        let WatchSignal::Changed(paths) = signal else {
            panic!("ordinary source paths must remain an incremental signal")
        };
        assert_eq!(
            paths,
            vec![
                PathBuf::from("project/Assets/Foo.prefab"),
                PathBuf::from("project/.unity-asset-search-ignore"),
            ]
        );
        assert!(!is_project_root_search_policy_path(
            Path::new("project"),
            Path::new("project/.gitignore")
        ));
    }

    #[test]
    fn an_ancestor_index_namespace_never_filters_project_events() {
        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![PathBuf::from("private-base/project/Assets/Foo.prefab")],
            attrs: notify::event::EventAttributes::new(),
        };

        let signal = watch_signal(event, Path::new("private-base/project"), None)
            .expect("an ancestor namespace must not suppress project events");
        let WatchSignal::Changed(paths) = signal else {
            panic!("ordinary source paths must remain an incremental signal")
        };

        assert_eq!(
            paths,
            [PathBuf::from("private-base/project/Assets/Foo.prefab")]
        );
    }

    #[tokio::test]
    async fn backend_rescan_without_paths_escalates_to_overflow_for_both_watchers() {
        let event =
            notify::Event::new(notify::EventKind::Any).set_flag(notify::event::Flag::Rescan);
        assert_eq!(
            watch_signal(event.clone(), Path::new("project"), None),
            Some(WatchSignal::Rescan)
        );
        assert_eq!(
            project_root_watch_signal(event, Path::new("project")),
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

    #[cfg(target_os = "macos")]
    #[test]
    fn watcher_recovers_apfs_case_aliases_for_deleted_namespace_events() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("Project");
        let namespace = project.join("Assets").join("SearchIndex");
        std::fs::create_dir_all(&namespace).unwrap();
        let project_alias = temporary.path().join("project");
        if !project_alias.exists() {
            return;
        }
        let deleted_alias = project_alias
            .join("assets")
            .join("searchindex")
            .join("deleted-generation.bin");
        let canonical_project = std::fs::canonicalize(&project).unwrap();
        let canonical_namespace = std::fs::canonicalize(&namespace).unwrap();

        assert!(platform_paths_equal(&project_alias, &canonical_project));
        assert!(platform_path_starts_with(
            &deleted_alias,
            &canonical_namespace
        ));

        let event = notify::Event {
            kind: notify::EventKind::Any,
            paths: vec![deleted_alias],
            attrs: notify::event::EventAttributes::new(),
        };
        assert!(
            watch_signal(event, &canonical_project, Some(&canonical_namespace)).is_none(),
            "an aliased deletion below the private namespace must stay filtered"
        );
    }

    #[cfg(windows)]
    #[test]
    fn watcher_matches_drive_and_verbatim_drive_path_aliases() {
        let ordinary = Path::new(r"C:\Project\Assets\SearchIndex\index-v1-a\state.bin");
        let verbatim_namespace = Path::new(r"\\?\c:\project\assets\searchindex");

        assert!(platform_path_starts_with(ordinary, verbatim_namespace));
        assert!(platform_path_starts_with(
            Path::new(r"\\?\C:\PROJECT\Assets\SearchIndex\state.bin"),
            Path::new(r"c:\project\assets\searchindex")
        ));
        assert!(is_project_root_search_policy_path(
            Path::new(r"\\?\C:\PROJECT"),
            Path::new(r"c:\project\.UNITY-ASSET-SEARCH-IGNORE")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn watcher_matches_unc_and_verbatim_unc_path_aliases() {
        assert!(platform_path_starts_with(
            Path::new(r"\\server\share\Project\Assets\SearchIndex\state.bin"),
            Path::new(r"\\?\UNC\SERVER\SHARE\project\assets\searchindex")
        ));
        assert!(platform_paths_equal(
            Path::new(r"\\?\UNC\server\share\Project"),
            Path::new(r"\\SERVER\SHARE\project")
        ));
    }

    #[tokio::test]
    async fn watcher_session_preserves_changed_paths_and_escalates_overflow() {
        let operations = ScriptedOperations::default();
        let (sender, scripted) = scripted_session();
        let ScriptedOpen::Session(events) = scripted else {
            unreachable!("scripted_session must return a session")
        };
        let mut session = ScriptedWatcherSession { events };
        let changed = vec![
            PathBuf::from("project/.unity-asset-search-ignore"),
            PathBuf::from("project/Assets/Foo.prefab"),
        ];
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
        let (event_sender, event_receiver) = mpsc::channel(1);
        let (failure_sender, failure_receiver) = watch::channel(None::<String>);
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        send_watch_signal(
            Some(WatchSignal::Changed(vec![PathBuf::from("Assets/A.asset")])),
            &event_sender,
            overflowed.as_ref(),
        );
        send_watch_signal(
            Some(WatchSignal::Changed(vec![PathBuf::from("Assets/B.asset")])),
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
        let (session_sender, session) = scripted_session();
        let factory = Arc::new(ScriptedWatcherFactory::new([
            ScriptedOpen::Fail("first initialization failure".to_owned()),
            ScriptedOpen::Fail("second initialization failure".to_owned()),
            session,
        ]));
        let operations = Arc::new(ScriptedOperations::default());
        let mut maintenance = MaintenanceRuntime::start_with_dependencies(
            operations,
            Some(watcher_config()),
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
        let factory = Arc::new(ScriptedWatcherFactory::new([ScriptedOpen::Fail(
            "watcher unavailable".to_owned(),
        )]));
        let operations = Arc::new(ScriptedOperations::default());
        let mut maintenance = MaintenanceRuntime::start_with_dependencies(
            operations.clone(),
            Some(watcher_config()),
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
        let (_session_sender, session) = scripted_session();
        let factory = Arc::new(ScriptedWatcherFactory::new([session]));
        let mut maintenance = MaintenanceRuntime::start_with_dependencies(
            Arc::new(ScriptedOperations::default()),
            Some(watcher_config()),
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
