//! Serial admission and execution for every daemon reindex trigger.
//!
//! Admission is intentionally centralized here. Filesystem events, timers, HTTP requests,
//! startup reconciliation, and workspace transactions must not grow independent scheduling
//! rules around the index builder.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::time::Instant;
use unity_asset_search_index::{
    DigestV1, DigestV1Builder, ReindexDisposition, ReindexIntent, ReindexReceipt, ReindexScope,
    SEARCH_GENERATION_CONTRACT_VERSION,
};

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(75);
const DEFAULT_MAX_DEBOUNCE: Duration = Duration::from_millis(750);
const DEFAULT_MAX_DIRTY_PATHS: usize = 4_096;
const DEFAULT_MAX_PENDING_EVENTS: usize = 32_768;
const DEFAULT_MAX_PENDING_TRANSACTIONS: usize = 1_024;
const DEFAULT_MAX_TRANSACTION_HISTORY: usize = 65_536;
const DEFAULT_MAX_FAILURE_HISTORY: usize = 64;
const MAX_FAILURE_MESSAGE_BYTES: usize = 4_096;

type BuildFuture = Pin<Box<dyn Future<Output = anyhow::Result<ReindexReceipt>> + Send + 'static>>;
type BuildExecutor = dyn Fn(ReindexIntent) -> BuildFuture + Send + Sync + 'static;
type CompletionOutcome = Result<ReindexReceipt, ExecutionFailure>;
type CompletionSender = oneshot::Sender<CompletionOutcome>;

/// The daemon boundary that admitted a reindex request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexSource {
    Startup,
    Watcher,
    Timer,
    Http,
    ChangeSet,
}

impl ReindexSource {
    const fn index(self) -> usize {
        match self {
            Self::Startup => 0,
            Self::Watcher => 1,
            Self::Timer => 2,
            Self::Http => 3,
            Self::ChangeSet => 4,
        }
    }
}

/// Bounded scheduling policy for [`ReindexCoordinator`].
#[derive(Debug, Clone)]
pub struct ReindexCoordinatorConfig {
    project_root: PathBuf,
    debounce: Duration,
    max_debounce: Duration,
    max_dirty_paths: usize,
    max_pending_events: usize,
    max_pending_transactions: usize,
    max_transaction_history: usize,
    max_failure_history: usize,
}

impl ReindexCoordinatorConfig {
    #[must_use]
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            debounce: DEFAULT_DEBOUNCE,
            max_debounce: DEFAULT_MAX_DEBOUNCE,
            max_dirty_paths: DEFAULT_MAX_DIRTY_PATHS,
            max_pending_events: DEFAULT_MAX_PENDING_EVENTS,
            max_pending_transactions: DEFAULT_MAX_PENDING_TRANSACTIONS,
            max_transaction_history: DEFAULT_MAX_TRANSACTION_HISTORY,
            max_failure_history: DEFAULT_MAX_FAILURE_HISTORY,
        }
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
    pub fn with_max_pending_events(mut self, maximum: usize) -> Self {
        self.max_pending_events = maximum;
        self
    }

    #[must_use]
    pub fn with_max_pending_transactions(mut self, maximum: usize) -> Self {
        self.max_pending_transactions = maximum;
        self
    }

    #[must_use]
    pub fn with_max_transaction_history(mut self, maximum: usize) -> Self {
        self.max_transaction_history = maximum;
        self
    }

    #[must_use]
    pub fn with_max_failure_history(mut self, maximum: usize) -> Self {
        self.max_failure_history = maximum;
        self
    }

    fn validate(mut self) -> Result<Self, CoordinatorError> {
        if !self.project_root.is_absolute() {
            return Err(CoordinatorError::InvalidConfiguration(
                "project_root must be absolute",
            ));
        }
        self.project_root = normalize_absolute_root(&self.project_root)?;
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
            ("max_pending_transactions", self.max_pending_transactions),
            ("max_transaction_history", self.max_transaction_history),
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
    ChangeSet,
}

impl ReindexScopeKind {
    const fn from_intent(intent: &ReindexIntent) -> Self {
        match &intent.scope {
            ReindexScope::Full => Self::Full,
            ReindexScope::Reconcile => Self::Reconcile,
            ReindexScope::ChangedPaths { .. } => Self::ChangedPaths,
            ReindexScope::ChangeSet { .. } => Self::ChangeSet,
        }
    }
}

/// One bounded diagnostic retained after a build failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexFailure {
    pub sequence: u64,
    pub scope: ReindexScopeKind,
    pub transaction: Option<String>,
    pub message: String,
}

/// Per-boundary admission counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReindexAdmissionCounts {
    pub startup: u64,
    pub watcher: u64,
    pub timer: u64,
    pub http: u64,
    pub change_set: u64,
}

impl ReindexAdmissionCounts {
    fn from_array(counts: [u64; 5]) -> Self {
        Self {
            startup: counts[0],
            watcher: counts[1],
            timer: counts[2],
            http: counts[3],
            change_set: counts[4],
        }
    }
}

/// A bounded point-in-time view of coordinator state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexCoordinatorSnapshot {
    pub running: bool,
    pub in_flight: Option<ReindexScopeKind>,
    pub pending_general: Option<ReindexScopeKind>,
    pub pending_transactions: usize,
    pub tracked_transactions: usize,
    pub applied_transactions: usize,
    pub failures: Vec<ReindexFailure>,
    pub full_escalations: u64,
    pub watcher_overflows: u64,
    pub admissions: ReindexAdmissionCounts,
}

impl ReindexCoordinatorSnapshot {
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        !self.running
            && self.in_flight.is_none()
            && self.pending_general.is_none()
            && self.pending_transactions == 0
    }
}

/// Initial admission and the terminal receipt produced by its merged filesystem build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexCompletion {
    /// Receipt returned by atomic admission before execution.
    pub admission: ReindexReceipt,
    /// Terminal receipt returned by the concrete executor.
    pub terminal: ReindexReceipt,
}

/// Concrete daemon-owned coordinator.
///
/// The executor is supplied once at construction but erased internally. Callers share this one
/// concrete type rather than defining parallel application-layer scheduling traits.
#[derive(Clone)]
pub struct ReindexCoordinator {
    inner: Arc<CoordinatorInner>,
}

impl fmt::Debug for ReindexCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReindexCoordinator")
            .field("project_root", &self.inner.config.project_root)
            .finish_non_exhaustive()
    }
}

impl ReindexCoordinator {
    pub fn new<F, Fut>(
        config: ReindexCoordinatorConfig,
        executor: F,
    ) -> Result<Self, CoordinatorError>
    where
        F: Fn(ReindexIntent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<ReindexReceipt>> + Send + 'static,
    {
        let config = config.validate()?;
        let executor: Arc<BuildExecutor> =
            Arc::new(move |intent| Box::pin(executor(intent)) as BuildFuture);
        Ok(Self {
            inner: Arc::new(CoordinatorInner {
                config,
                executor,
                state: Mutex::new(CoordinatorState::default()),
                wake: Notify::new(),
                changed: Notify::new(),
            }),
        })
    }

    /// Atomically admits, rejects, or coalesces one reindex request.
    pub async fn admit(
        &self,
        source: ReindexSource,
        intent: ReindexIntent,
    ) -> Result<ReindexReceipt, CoordinatorError> {
        self.admit_inner(source, intent, false).await
    }

    /// Atomically admits one filesystem-backed request and waits for its merged build result.
    ///
    /// Change-set requests require an authoritative workspace view and are rejected by this API.
    pub async fn admit_and_wait(
        &self,
        source: ReindexSource,
        intent: ReindexIntent,
    ) -> Result<ReindexCompletion, CoordinatorError> {
        if intent.contract_version != SEARCH_GENERATION_CONTRACT_VERSION {
            return Err(CoordinatorError::UnsupportedContractVersion {
                actual: intent.contract_version,
                expected: SEARCH_GENERATION_CONTRACT_VERSION,
            });
        }
        if matches!(&intent.scope, ReindexScope::ChangeSet { .. }) {
            return Err(CoordinatorError::SynchronousChangeSetUnsupported);
        }

        let normalized = normalize_general_scope(&intent, &self.inner.config)?;
        let (completion_sender, completion_receiver) = oneshot::channel();
        let (admission, should_start) = {
            let mut state = self.inner.state.lock().await;
            state.admissions[source.index()] = state.admissions[source.index()].saturating_add(1);
            let escalated = normalized.escalated;
            let disposition = state.admit_general(
                normalized.scope,
                Instant::now(),
                &self.inner.config,
                escalated,
                Some(completion_sender),
            )?;
            if escalated {
                state.full_escalations = state.full_escalations.saturating_add(1);
            }
            let should_start = state.start_runner_if_needed();
            (admission_receipt(disposition, &intent), should_start)
        };

        self.signal_runner_after_admission(should_start);
        let completion = match completion_receiver.await {
            Ok(completion) => completion,
            Err(_) => {
                return Err(CoordinatorError::CompletionChannelClosed {
                    admission: Box::new(admission),
                });
            }
        };
        match completion {
            Ok(terminal) => Ok(ReindexCompletion {
                admission,
                terminal,
            }),
            Err(failure) => Err(CoordinatorError::ExecutionFailed {
                admission: Box::new(admission),
                scope: failure.scope,
                message: failure.message,
            }),
        }
    }

    /// Records a lossy watcher overflow and upgrades pending filesystem work to a full scan.
    pub async fn watcher_overflow(&self) -> Result<ReindexReceipt, CoordinatorError> {
        self.admit_inner(ReindexSource::Watcher, ReindexIntent::full(), true)
            .await
    }

    /// Waits until the runner has drained every admitted request.
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

    async fn admit_inner(
        &self,
        source: ReindexSource,
        intent: ReindexIntent,
        watcher_overflow: bool,
    ) -> Result<ReindexReceipt, CoordinatorError> {
        if intent.contract_version != SEARCH_GENERATION_CONTRACT_VERSION {
            return Err(CoordinatorError::UnsupportedContractVersion {
                actual: intent.contract_version,
                expected: SEARCH_GENERATION_CONTRACT_VERSION,
            });
        }
        let transaction = match &intent.scope {
            ReindexScope::ChangeSet { .. } => Some(transaction_record(&intent)?),
            ReindexScope::Full | ReindexScope::Reconcile | ReindexScope::ChangedPaths { .. } => {
                None
            }
        };

        let mut state = self.inner.state.lock().await;
        state.admissions[source.index()] = state.admissions[source.index()].saturating_add(1);

        let disposition = if watcher_overflow {
            state.watcher_overflows = state.watcher_overflows.saturating_add(1);
            state.full_escalations = state.full_escalations.saturating_add(1);
            state.admit_general(
                GeneralScope::Full,
                Instant::now(),
                &self.inner.config,
                true,
                None,
            )?
        } else if let Some(transaction) = transaction {
            state.admit_transaction(&intent, transaction, &self.inner.config)?
        } else {
            let scope = normalize_general_scope(&intent, &self.inner.config)?;
            if scope.escalated {
                state.full_escalations = state.full_escalations.saturating_add(1);
            }
            state.admit_general(
                scope.scope,
                Instant::now(),
                &self.inner.config,
                scope.escalated,
                None,
            )?
        };

        let should_start = state.start_runner_if_needed();
        let receipt = admission_receipt(disposition, &intent);
        drop(state);

        self.signal_runner_after_admission(should_start);
        Ok(receipt)
    }

    fn signal_runner_after_admission(&self, should_start: bool) {
        self.inner.changed.notify_waiters();
        self.inner.wake.notify_one();
        if should_start {
            let inner = Arc::clone(&self.inner);
            let _runner = tokio::spawn(async move {
                run_coordinator(inner).await;
            });
        }
    }
}

struct CoordinatorInner {
    config: ReindexCoordinatorConfig,
    executor: Arc<BuildExecutor>,
    state: Mutex<CoordinatorState>,
    wake: Notify,
    changed: Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransactionBinding {
    workspace: u128,
    from_revision: DigestV1,
    to_revision: DigestV1,
    change_set_digest: DigestV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransactionRecord {
    transaction: DigestV1,
    binding: TransactionBinding,
}

#[derive(Debug)]
struct PendingTransaction {
    intent: ReindexIntent,
    record: TransactionRecord,
}

#[derive(Default)]
struct CoordinatorState {
    runner_running: bool,
    in_flight: Option<ReindexIntent>,
    pending_general: Option<PendingGeneral>,
    transaction_queue: VecDeque<PendingTransaction>,
    tracked_transactions: BTreeMap<DigestV1, TransactionBinding>,
    applied_transactions: BTreeMap<DigestV1, TransactionBinding>,
    applied_order: VecDeque<DigestV1>,
    failures: VecDeque<ReindexFailure>,
    failure_sequence: u64,
    full_escalations: u64,
    watcher_overflows: u64,
    admissions: [u64; 5],
    completion_waiter_count: usize,
}

impl CoordinatorState {
    fn admit_transaction(
        &mut self,
        intent: &ReindexIntent,
        record: TransactionRecord,
        config: &ReindexCoordinatorConfig,
    ) -> Result<ReindexDisposition, CoordinatorError> {
        if let Some(existing) = self.applied_transactions.get(&record.transaction) {
            return classify_duplicate(record, *existing, ReindexDisposition::AlreadyApplied);
        }
        if let Some(existing) = self.tracked_transactions.get(&record.transaction) {
            return classify_duplicate(record, *existing, ReindexDisposition::Coalesced);
        }
        if self.transaction_queue.len() >= config.max_pending_transactions {
            return Err(CoordinatorError::TransactionQueueFull {
                maximum: config.max_pending_transactions,
            });
        }

        self.tracked_transactions
            .insert(record.transaction, record.binding);
        self.transaction_queue.push_back(PendingTransaction {
            intent: intent.clone(),
            record,
        });
        Ok(ReindexDisposition::Queued)
    }

    fn admit_general(
        &mut self,
        incoming: GeneralScope,
        now: Instant,
        config: &ReindexCoordinatorConfig,
        force_immediate: bool,
        completion_waiter: Option<CompletionSender>,
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

        if let Some(waiter) = completion_waiter {
            pending.try_push_waiter(waiter)?;
            self.completion_waiter_count += 1;
        }
        pending.last_event = now;
        pending.event_count = pending.event_count.saturating_add(1);
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
        let should_start = !self.runner_running
            && (self.pending_general.is_some() || !self.transaction_queue.is_empty());
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
            let intent = scope.into_intent();
            self.in_flight = Some(intent.clone());
            return RunnerAction::Execute {
                intent: Box::new(intent),
                transaction: None,
                waiters,
            };
        }

        if let Some(transaction) = self.transaction_queue.pop_front() {
            self.in_flight = Some(transaction.intent.clone());
            return RunnerAction::Execute {
                intent: Box::new(transaction.intent),
                transaction: Some(transaction.record),
                waiters: Vec::new(),
            };
        }

        if let Some(pending) = self.pending_general.as_ref() {
            return RunnerAction::WaitUntil(pending.ready_at(config));
        }

        self.runner_running = false;
        RunnerAction::Stop
    }

    fn finish(
        &mut self,
        intent: &ReindexIntent,
        transaction: Option<TransactionRecord>,
        outcome: &CompletionOutcome,
        completed_waiters: usize,
        config: &ReindexCoordinatorConfig,
    ) {
        self.in_flight = None;
        self.completion_waiter_count = self
            .completion_waiter_count
            .saturating_sub(completed_waiters);
        let Some(record) = transaction else {
            if let Err(failure) = outcome {
                self.record_failure(intent, failure.message.clone(), config.max_failure_history);
            }
            return;
        };

        self.tracked_transactions.remove(&record.transaction);
        match outcome {
            Ok(_) => self.record_applied(record, config.max_transaction_history),
            Err(failure) => {
                self.record_failure(intent, failure.message.clone(), config.max_failure_history)
            }
        }
    }

    fn record_applied(&mut self, record: TransactionRecord, maximum: usize) {
        if self
            .applied_transactions
            .insert(record.transaction, record.binding)
            .is_none()
        {
            self.applied_order.push_back(record.transaction);
        }
        while self.applied_order.len() > maximum {
            if let Some(evicted) = self.applied_order.pop_front() {
                self.applied_transactions.remove(&evicted);
            }
        }
    }

    fn record_failure(&mut self, intent: &ReindexIntent, message: String, maximum: usize) {
        self.failure_sequence = self.failure_sequence.saturating_add(1);
        self.failures.push_back(ReindexFailure {
            sequence: self.failure_sequence,
            scope: ReindexScopeKind::from_intent(intent),
            transaction: transaction_key(intent).map(|transaction| transaction.to_string()),
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
            pending_transactions: self.transaction_queue.len(),
            tracked_transactions: self.tracked_transactions.len(),
            applied_transactions: self.applied_transactions.len(),
            failures: self.failures.iter().cloned().collect(),
            full_escalations: self.full_escalations,
            watcher_overflows: self.watcher_overflows,
            admissions: ReindexAdmissionCounts::from_array(self.admissions),
        }
    }
}

enum RunnerAction {
    Execute {
        intent: Box<ReindexIntent>,
        transaction: Option<TransactionRecord>,
        waiters: Vec<CompletionSender>,
    },
    WaitUntil(Instant),
    Stop,
}

async fn run_coordinator(inner: Arc<CoordinatorInner>) {
    loop {
        let notified = inner.wake.notified();
        let action = {
            let mut state = inner.state.lock().await;
            state.take_ready(Instant::now(), &inner.config)
        };

        match action {
            RunnerAction::Execute {
                intent,
                transaction,
                waiters,
            } => {
                inner.changed.notify_waiters();
                let outcome = execute(&inner.executor, &intent).await;
                let mut state = inner.state.lock().await;
                state.finish(&intent, transaction, &outcome, waiters.len(), &inner.config);
                drop(state);
                for waiter in waiters {
                    let _receiver_was_dropped = waiter.send(outcome.clone());
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
                return;
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
    fn new(intent: &ReindexIntent, message: String) -> Self {
        Self {
            scope: ReindexScopeKind::from_intent(intent),
            message: truncate_message(message),
        }
    }
}

async fn execute(executor: &Arc<BuildExecutor>, intent: &ReindexIntent) -> CompletionOutcome {
    let outcome = match catch_unwind(AssertUnwindSafe(|| executor(intent.clone()))) {
        Ok(future) => {
            let validation_intent = intent.clone();
            match tokio::spawn(async move {
                match future.await {
                    Ok(receipt) => validate_execution_receipt(&validation_intent, receipt),
                    Err(error) => Err(error.to_string()),
                }
            })
            .await
            {
                Ok(outcome) => outcome,
                Err(join_error) => Err(format!("reindex build task failed: {join_error}")),
            }
        }
        Err(_) => Err("reindex executor panicked before returning its future".to_owned()),
    };
    outcome.map_err(|message| ExecutionFailure::new(intent, message))
}

fn validate_execution_receipt(
    intent: &ReindexIntent,
    receipt: ReindexReceipt,
) -> Result<ReindexReceipt, String> {
    if receipt.contract_version != SEARCH_GENERATION_CONTRACT_VERSION {
        return Err(format!(
            "executor returned receipt contract version {}, expected {}",
            receipt.contract_version, SEARCH_GENERATION_CONTRACT_VERSION
        ));
    }
    if receipt.transaction != intent.scope.transaction() {
        return Err("executor returned a receipt for a different transaction".to_owned());
    }
    if receipt.target_revision != intent.scope.target_revision() {
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
    Ok(receipt)
}

#[derive(Debug)]
struct NormalizedGeneralScope {
    scope: GeneralScope,
    escalated: bool,
}

fn normalize_general_scope(
    intent: &ReindexIntent,
    config: &ReindexCoordinatorConfig,
) -> Result<NormalizedGeneralScope, CoordinatorError> {
    match &intent.scope {
        ReindexScope::Full => Ok(NormalizedGeneralScope {
            scope: GeneralScope::Full,
            escalated: false,
        }),
        ReindexScope::Reconcile => Ok(NormalizedGeneralScope {
            scope: GeneralScope::Reconcile,
            escalated: false,
        }),
        ReindexScope::ChangedPaths { paths } => {
            let mut normalized = BTreeSet::new();
            for path in paths {
                let Some(path) = normalize_changed_path(&config.project_root, path)? else {
                    return Ok(NormalizedGeneralScope {
                        scope: GeneralScope::Full,
                        escalated: true,
                    });
                };
                normalized.insert(path);
                if normalized.len() > config.max_dirty_paths {
                    return Ok(NormalizedGeneralScope {
                        scope: GeneralScope::Full,
                        escalated: true,
                    });
                }
            }
            if normalized.is_empty() {
                return Ok(NormalizedGeneralScope {
                    scope: GeneralScope::Reconcile,
                    escalated: false,
                });
            }
            Ok(NormalizedGeneralScope {
                scope: GeneralScope::ChangedPaths(normalized),
                escalated: false,
            })
        }
        ReindexScope::ChangeSet { .. } => Err(CoordinatorError::MissingTransaction),
    }
}

#[derive(Debug)]
enum GeneralScope {
    Full,
    Reconcile,
    ChangedPaths(BTreeSet<PathBuf>),
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
                    current.extend(incoming);
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

    fn into_intent(self) -> ReindexIntent {
        match self {
            Self::Full => ReindexIntent::full(),
            Self::Reconcile => ReindexIntent::reconcile(),
            Self::ChangedPaths(paths) => ReindexIntent::changed_paths(paths.into_iter().collect()),
        }
    }
}

#[derive(Debug)]
struct PendingGeneral {
    scope: GeneralScope,
    first_admitted: Instant,
    last_event: Instant,
    event_count: usize,
    force_immediate: bool,
    waiters: Vec<CompletionSender>,
}

impl PendingGeneral {
    fn new(scope: GeneralScope, now: Instant, force_immediate: bool) -> Self {
        Self {
            scope,
            first_admitted: now,
            last_event: now,
            event_count: 1,
            force_immediate,
            waiters: Vec::new(),
        }
    }

    fn try_push_waiter(&mut self, waiter: CompletionSender) -> Result<(), CoordinatorError> {
        self.waiters
            .try_reserve(1)
            .map_err(|_| CoordinatorError::CompletionWaiterAllocationFailed)?;
        self.waiters.push(waiter);
        Ok(())
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

fn normalize_absolute_root(root: &Path) -> Result<PathBuf, CoordinatorError> {
    let mut normalized = PathBuf::new();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(CoordinatorError::InvalidConfiguration(
                        "project_root escapes its filesystem root",
                    ));
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    Ok(normalized)
}

fn normalize_changed_path(
    project_root: &Path,
    supplied: &Path,
) -> Result<Option<PathBuf>, CoordinatorError> {
    let relative = if supplied.is_absolute() {
        supplied
            .strip_prefix(project_root)
            .map_err(|_| CoordinatorError::PathOutsideProject {
                path: supplied.to_path_buf(),
                project_root: project_root.to_path_buf(),
            })?
    } else {
        supplied
    };

    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => normalized.push(segment),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(CoordinatorError::PathOutsideProject {
                        path: supplied.to_path_buf(),
                        project_root: project_root.to_path_buf(),
                    });
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(CoordinatorError::PathOutsideProject {
                    path: supplied.to_path_buf(),
                    project_root: project_root.to_path_buf(),
                });
            }
        }
    }
    Ok((!normalized.as_os_str().is_empty()).then_some(normalized))
}

fn transaction_record(intent: &ReindexIntent) -> Result<TransactionRecord, CoordinatorError> {
    let ReindexScope::ChangeSet { changes } = &intent.scope else {
        return Err(CoordinatorError::MissingTransaction);
    };
    let change_set_digest = canonical_json_digest(changes)?;
    Ok(TransactionRecord {
        transaction: changes.transaction().digest(),
        binding: TransactionBinding {
            workspace: changes.workspace().get(),
            from_revision: changes.from_revision().digest(),
            to_revision: changes.to_revision().digest(),
            change_set_digest,
        },
    })
}

fn classify_duplicate(
    incoming: TransactionRecord,
    existing: TransactionBinding,
    disposition: ReindexDisposition,
) -> Result<ReindexDisposition, CoordinatorError> {
    if incoming.binding == existing {
        Ok(disposition)
    } else {
        Err(CoordinatorError::TransactionConflict {
            transaction: incoming.transaction,
            existing_change_set: existing.change_set_digest,
            incoming_change_set: incoming.binding.change_set_digest,
        })
    }
}

fn canonical_json_digest(value: &impl Serialize) -> Result<DigestV1, CoordinatorError> {
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| CoordinatorError::TransactionBinding(error.to_string()))?;

    let mut builder = DigestV1Builder::new(counter.bytes);
    serde_json::to_writer(DigestWriter(&mut builder), value)
        .map_err(|error| CoordinatorError::TransactionBinding(error.to_string()))?;
    builder
        .finalize()
        .map_err(|error| CoordinatorError::TransactionBinding(error.to_string()))
}

#[derive(Default)]
struct JsonByteCounter {
    bytes: u64,
}

impl Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let amount = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("canonical ChangeSet JSON length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(amount)
            .ok_or_else(|| io::Error::other("canonical ChangeSet JSON length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DigestWriter<'builder>(&'builder mut DigestV1Builder);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer).map_err(io::Error::other)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn transaction_key(intent: &ReindexIntent) -> Option<DigestV1> {
    intent
        .scope
        .transaction()
        .map(|transaction| transaction.digest())
}

fn admission_receipt(disposition: ReindexDisposition, intent: &ReindexIntent) -> ReindexReceipt {
    ReindexReceipt {
        contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
        disposition,
        transaction: intent.scope.transaction(),
        target_revision: intent.scope.target_revision(),
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
    UnsupportedContractVersion {
        actual: u16,
        expected: u16,
    },
    SynchronousChangeSetUnsupported,
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
    MissingTransaction,
    TransactionBinding(String),
    TransactionConflict {
        transaction: DigestV1,
        existing_change_set: DigestV1,
        incoming_change_set: DigestV1,
    },
    PathOutsideProject {
        path: PathBuf,
        project_root: PathBuf,
    },
    TransactionQueueFull {
        maximum: usize,
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
            Self::UnsupportedContractVersion { actual, expected } => write!(
                formatter,
                "reindex intent contract version {actual} is unsupported; expected {expected}"
            ),
            Self::SynchronousChangeSetUnsupported => formatter
                .write_str("synchronous completion is unavailable for ChangeSet reindex requests"),
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
            Self::MissingTransaction => {
                formatter.write_str("ChangeSet reindex intent is missing its transaction")
            }
            Self::TransactionBinding(message) => {
                write!(formatter, "could not bind ChangeSet transaction: {message}")
            }
            Self::TransactionConflict {
                transaction,
                existing_change_set,
                incoming_change_set,
            } => write!(
                formatter,
                "transaction {transaction} was reused for a different ChangeSet \
                 (existing {existing_change_set}, incoming {incoming_change_set})"
            ),
            Self::PathOutsideProject { path, project_root } => write!(
                formatter,
                "changed path {} is outside project root {}",
                path.display(),
                project_root.display()
            ),
            Self::TransactionQueueFull { maximum } => write!(
                formatter,
                "reindex transaction queue is full; maximum pending transactions is {maximum}"
            ),
        }
    }
}

impl Error for CoordinatorError {}
