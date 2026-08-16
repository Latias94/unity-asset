use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeSnapshot {
    pub(crate) active_working_set_bytes: u64,
    pub(crate) peak_working_set_bytes: u64,
    pub(crate) active_open_files: usize,
    pub(crate) peak_open_files: usize,
    pub(crate) preflight_hash_bytes: u64,
    pub(crate) started_ordinals: Vec<u32>,
    pub(crate) waiting_ordinals: Vec<u32>,
    pub(crate) failed_ordinals: Vec<u32>,
}

#[derive(Debug)]
pub(crate) struct ExecutionProbe {
    block_ordinals: Vec<u32>,
    fail_ordinals: Vec<u32>,
    state: Mutex<ProbeState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct ProbeState {
    active_working_set_bytes: u64,
    peak_working_set_bytes: u64,
    active_open_files: usize,
    peak_open_files: usize,
    preflight_hash_bytes: u64,
    started_ordinals: Vec<u32>,
    first_write_ordinals: Vec<u32>,
    waiting_ordinals: Vec<u32>,
    failed_ordinals: Vec<u32>,
    writes_released: bool,
    block_publication_commit: bool,
    publication_commit_waiting: bool,
    publication_commit_released: bool,
}

impl ExecutionProbe {
    pub(crate) fn new(
        block_ordinals: impl IntoIterator<Item = u32>,
        fail_ordinals: impl IntoIterator<Item = u32>,
    ) -> Arc<Self> {
        Arc::new(Self {
            block_ordinals: block_ordinals.into_iter().collect(),
            fail_ordinals: fail_ordinals.into_iter().collect(),
            state: Mutex::new(ProbeState::default()),
            changed: Condvar::new(),
        })
    }

    pub(crate) fn wait_for_waiters(&self, count: usize) {
        self.wait_until("blocked extraction writers", |state| {
            state.waiting_ordinals.len() >= count
        });
    }

    pub(crate) fn wait_for_failures(&self, count: usize) {
        self.wait_until("injected extraction failures", |state| {
            state.failed_ordinals.len() >= count
        });
    }

    pub(crate) fn release_writes(&self) {
        let mut state = lock_recover(&self.state);
        state.writes_released = true;
        self.changed.notify_all();
    }

    pub(crate) fn block_publication_commit(&self) {
        let mut state = lock_recover(&self.state);
        state.block_publication_commit = true;
    }

    pub(crate) fn wait_for_publication_commit(&self) {
        self.wait_until("publication commit", |state| {
            state.publication_commit_waiting
        });
    }

    pub(crate) fn release_publication_commit(&self) {
        let mut state = lock_recover(&self.state);
        state.publication_commit_released = true;
        self.changed.notify_all();
    }

    pub(crate) fn release_on_drop(self: &Arc<Self>) -> ProbeReleaseGuard {
        ProbeReleaseGuard {
            probe: Arc::clone(self),
        }
    }

    pub(crate) fn snapshot(&self) -> ProbeSnapshot {
        let state = lock_recover(&self.state);
        ProbeSnapshot {
            active_working_set_bytes: state.active_working_set_bytes,
            peak_working_set_bytes: state.peak_working_set_bytes,
            active_open_files: state.active_open_files,
            peak_open_files: state.peak_open_files,
            preflight_hash_bytes: state.preflight_hash_bytes,
            started_ordinals: state.started_ordinals.clone(),
            waiting_ordinals: state.waiting_ordinals.clone(),
            failed_ordinals: state.failed_ordinals.clone(),
        }
    }

    fn wait_until(&self, description: &str, ready: impl Fn(&ProbeState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = lock_recover(&self.state);
        while !ready(&state) {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for {description}");
            let timeout = deadline.saturating_duration_since(now);
            let (next, result) = self
                .changed
                .wait_timeout(state, timeout)
                .unwrap_or_else(|error| error.into_inner());
            state = next;
            assert!(
                !result.timed_out() || ready(&state),
                "timed out waiting for {description}"
            );
        }
    }

    fn enter_work(self: &Arc<Self>, ordinal: u32, working_set_bytes: u64) -> WorkGuard {
        let mut state = lock_recover(&self.state);
        state.active_working_set_bytes = state
            .active_working_set_bytes
            .checked_add(working_set_bytes)
            .expect("test working-set accounting must not overflow");
        state.peak_working_set_bytes = state
            .peak_working_set_bytes
            .max(state.active_working_set_bytes);
        state.started_ordinals.push(ordinal);
        self.changed.notify_all();
        WorkGuard {
            probe: Some(Arc::clone(self)),
            working_set_bytes,
        }
    }

    fn reserve_open_files(self: &Arc<Self>, count: usize) -> OpenFileGuard {
        let mut state = lock_recover(&self.state);
        state.active_open_files = state
            .active_open_files
            .checked_add(count)
            .expect("test open-file accounting must not overflow");
        state.peak_open_files = state.peak_open_files.max(state.active_open_files);
        self.changed.notify_all();
        OpenFileGuard {
            probe: Some(Arc::clone(self)),
            count,
        }
    }

    fn record_preflight_hash(&self, bytes: u64) {
        let mut state = lock_recover(&self.state);
        state.preflight_hash_bytes = state
            .preflight_hash_bytes
            .checked_add(bytes)
            .expect("test existing-output hash accounting must not overflow");
        self.changed.notify_all();
    }

    fn before_write(&self, ordinal: u32) -> io::Result<()> {
        let mut state = lock_recover(&self.state);
        if state.first_write_ordinals.contains(&ordinal) {
            return Ok(());
        }
        state.first_write_ordinals.push(ordinal);
        if self.fail_ordinals.contains(&ordinal) {
            state.failed_ordinals.push(ordinal);
            self.changed.notify_all();
            return Err(io::Error::other("injected extraction sink failure"));
        }
        if self.block_ordinals.contains(&ordinal) {
            state.waiting_ordinals.push(ordinal);
            self.changed.notify_all();
            while !state.writes_released {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
        Ok(())
    }

    fn before_publication_commit(&self) {
        let mut state = lock_recover(&self.state);
        if !state.block_publication_commit {
            return;
        }
        state.publication_commit_waiting = true;
        self.changed.notify_all();
        while !state.publication_commit_released {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

pub(crate) struct ProbeReleaseGuard {
    probe: Arc<ExecutionProbe>,
}

impl Drop for ProbeReleaseGuard {
    fn drop(&mut self) {
        self.probe.release_writes();
    }
}

pub(crate) struct WorkGuard {
    probe: Option<Arc<ExecutionProbe>>,
    working_set_bytes: u64,
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        let Some(probe) = self.probe.take() else {
            return;
        };
        let mut state = lock_recover(&probe.state);
        state.active_working_set_bytes = state
            .active_working_set_bytes
            .checked_sub(self.working_set_bytes)
            .expect("test working-set accounting must remain balanced");
        probe.changed.notify_all();
    }
}

pub(crate) struct OpenFileGuard {
    probe: Option<Arc<ExecutionProbe>>,
    count: usize,
}

impl Drop for OpenFileGuard {
    fn drop(&mut self) {
        let Some(probe) = self.probe.take() else {
            return;
        };
        let mut state = lock_recover(&probe.state);
        state.active_open_files = state
            .active_open_files
            .checked_sub(self.count)
            .expect("test open-file accounting must remain balanced");
        probe.changed.notify_all();
    }
}

pub(crate) fn enter_work(
    probe: Option<&Arc<ExecutionProbe>>,
    ordinal: u32,
    working_set_bytes: u64,
) -> WorkGuard {
    probe.map_or(
        WorkGuard {
            probe: None,
            working_set_bytes: 0,
        },
        |probe| probe.enter_work(ordinal, working_set_bytes),
    )
}

pub(crate) fn reserve_open_files(
    probe: Option<&Arc<ExecutionProbe>>,
    count: usize,
) -> OpenFileGuard {
    probe.map_or(
        OpenFileGuard {
            probe: None,
            count: 0,
        },
        |probe| probe.reserve_open_files(count),
    )
}

pub(crate) fn record_preflight_hash(probe: Option<&Arc<ExecutionProbe>>, bytes: u64) {
    if let Some(probe) = probe {
        probe.record_preflight_hash(bytes);
    }
}

pub(crate) fn before_publication_commit(probe: Option<&Arc<ExecutionProbe>>) {
    if let Some(probe) = probe {
        probe.before_publication_commit();
    }
}

pub(crate) struct ObservedWriter<'writer> {
    probe: Option<Arc<ExecutionProbe>>,
    ordinal: u32,
    inner: &'writer mut dyn Write,
}

impl<'writer> ObservedWriter<'writer> {
    pub(crate) fn new(
        probe: Option<&Arc<ExecutionProbe>>,
        ordinal: u32,
        inner: &'writer mut dyn Write,
    ) -> Self {
        Self {
            probe: probe.cloned(),
            ordinal,
            inner,
        }
    }
}

impl Write for ObservedWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Some(probe) = self.probe.as_ref() {
            probe.before_write(self.ordinal)?;
        }
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
