use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Barrier, Mutex, Semaphore, mpsc};
use unity_asset_core::{DigestV1, WorkspaceId, WorkspaceRevision};
use unity_asset_search_daemon::coordinator::{
    CoordinatorError, FilesystemReindexIntent, FilesystemReindexScope, ReindexCoordinator,
    ReindexCoordinatorConfig, ReindexScopeKind, ReindexSource,
};
use unity_asset_search_index::{
    GenerationStamp, ReindexDisposition, ReindexIntent, ReindexReceipt, ReindexScope,
    SEARCH_GENERATION_CONTRACT_VERSION, SearchGenerationId,
};

fn project_root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\unity-asset-coordinator-tests")
    } else {
        PathBuf::from("/unity-asset-coordinator-tests")
    }
}

fn config() -> ReindexCoordinatorConfig {
    ReindexCoordinatorConfig::new(project_root())
        .with_debounce(Duration::from_millis(100))
        .with_max_debounce(Duration::from_millis(500))
}

fn terminal_receipt(intent: &ReindexIntent) -> ReindexReceipt {
    ReindexReceipt {
        contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
        disposition: ReindexDisposition::Applied,
        transaction: intent.scope.transaction(),
        target_revision: intent.scope.target_revision(),
        generation: None,
        evidence: Default::default(),
    }
}

fn changed(path: impl AsRef<Path>) -> FilesystemReindexIntent {
    FilesystemReindexIntent::changed_paths(vec![path.as_ref().to_path_buf()])
}

async fn wait_for_idle(coordinator: &ReindexCoordinator) {
    tokio::time::timeout(Duration::from_secs(5), coordinator.wait_for_idle())
        .await
        .expect("coordinator must become idle before the test deadline");
}

async fn wait_at_barrier(barrier: &Barrier) {
    tokio::time::timeout(Duration::from_secs(5), barrier.wait())
        .await
        .expect("barrier must open before the test deadline");
}

async fn wait_for_http_admissions(coordinator: &ReindexCoordinator, expected: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if coordinator.snapshot().await.admissions.http >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("HTTP admissions must be observed before the test deadline");
}

#[test]
fn unrepresentable_debounce_deadline_is_rejected_before_runner_start() {
    let result = ReindexCoordinator::new(
        ReindexCoordinatorConfig::new(project_root()).with_max_debounce(Duration::MAX),
        |intent| async move { Ok(terminal_receipt(&intent)) },
    );
    assert!(matches!(
        result,
        Err(CoordinatorError::InvalidConfiguration(_))
    ));
}

#[tokio::test]
async fn four_filesystem_entry_points_share_one_atomic_admission_and_one_build() {
    let builds = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let overlap = Arc::new(AtomicBool::new(false));
    let executed = Arc::new(Mutex::new(Vec::new()));
    let coordinator = ReindexCoordinator::new(config(), {
        let builds = Arc::clone(&builds);
        let active = Arc::clone(&active);
        let overlap = Arc::clone(&overlap);
        let executed = Arc::clone(&executed);
        move |intent| {
            let builds = Arc::clone(&builds);
            let active = Arc::clone(&active);
            let overlap = Arc::clone(&overlap);
            let executed = Arc::clone(&executed);
            async move {
                builds.fetch_add(1, Ordering::SeqCst);
                if active.fetch_add(1, Ordering::SeqCst) != 0 {
                    overlap.store(true, Ordering::SeqCst);
                }
                executed.lock().await.push(intent.clone());
                tokio::time::sleep(Duration::from_millis(20)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");

    let start = Arc::new(Barrier::new(5));
    let requests = [
        (
            ReindexSource::Startup,
            project_root().join("Assets/startup.prefab"),
        ),
        (
            ReindexSource::Watcher,
            PathBuf::from("Assets/nested/../watcher.prefab"),
        ),
        (ReindexSource::Timer, PathBuf::from("./Assets/timer.prefab")),
        (ReindexSource::Http, PathBuf::from("Assets/http.prefab")),
    ]
    .into_iter()
    .map(|(source, path)| {
        let coordinator = coordinator.clone();
        let start = Arc::clone(&start);
        tokio::spawn(async move {
            wait_at_barrier(&start).await;
            coordinator
                .admit(source, changed(&path))
                .await
                .expect("admission must succeed")
        })
    })
    .collect::<Vec<_>>();
    wait_at_barrier(&start).await;

    let mut dispositions = Vec::new();
    for request in requests {
        dispositions.push(
            request
                .await
                .expect("request task must complete")
                .disposition,
        );
    }
    wait_for_idle(&coordinator).await;

    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert!(!overlap.load(Ordering::SeqCst));
    assert_eq!(
        dispositions
            .iter()
            .filter(|&&value| value == ReindexDisposition::Queued)
            .count(),
        1
    );
    assert_eq!(
        dispositions
            .iter()
            .filter(|&&value| value == ReindexDisposition::Coalesced)
            .count(),
        3
    );

    let executed = executed.lock().await;
    let ReindexScope::ChangedPaths { paths } = &executed[0].scope else {
        panic!("four changed-path requests must remain one changed-path build");
    };
    assert_eq!(
        paths,
        &[
            PathBuf::from("Assets/http.prefab"),
            PathBuf::from("Assets/startup.prefab"),
            PathBuf::from("Assets/timer.prefab"),
            PathBuf::from("Assets/watcher.prefab"),
        ]
    );
    let snapshot = coordinator.snapshot().await;
    assert_eq!(snapshot.admissions.startup, 1);
    assert_eq!(snapshot.admissions.watcher, 1);
    assert_eq!(snapshot.admissions.timer, 1);
    assert_eq!(snapshot.admissions.http, 1);
}

#[tokio::test]
async fn full_scope_absorbs_pending_reconcile_and_changed_paths() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let coordinator = ReindexCoordinator::new(config(), {
        let executed = Arc::clone(&executed);
        move |intent| {
            let executed = Arc::clone(&executed);
            async move {
                executed.lock().await.push(intent.clone());
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");

    assert_eq!(
        coordinator
            .admit(ReindexSource::Watcher, changed("Assets/a.prefab"))
            .await
            .expect("changed path must queue")
            .disposition,
        ReindexDisposition::Queued
    );
    assert_eq!(
        coordinator
            .admit(ReindexSource::Timer, FilesystemReindexIntent::reconcile(),)
            .await
            .expect("reconcile must merge")
            .disposition,
        ReindexDisposition::Coalesced
    );
    assert_eq!(
        coordinator
            .admit(ReindexSource::Http, FilesystemReindexIntent::full())
            .await
            .expect("full must merge")
            .disposition,
        ReindexDisposition::Coalesced
    );

    wait_for_idle(&coordinator).await;
    let executed = executed.lock().await;
    assert_eq!(executed.len(), 1);
    assert!(matches!(&executed[0].scope, ReindexScope::Full));
}

#[tokio::test]
async fn dirty_path_limits_and_watcher_overflow_upgrade_to_full() {
    let threshold_executed = Arc::new(Mutex::new(Vec::new()));
    let threshold_coordinator = ReindexCoordinator::new(config().with_max_dirty_paths(2), {
        let threshold_executed = Arc::clone(&threshold_executed);
        move |intent| {
            let threshold_executed = Arc::clone(&threshold_executed);
            async move {
                threshold_executed.lock().await.push(intent.clone());
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("threshold coordinator must be constructible");
    for path in ["Assets/a.prefab", "Assets/b.prefab", "Assets/c.prefab"] {
        threshold_coordinator
            .admit(ReindexSource::Watcher, changed(path))
            .await
            .expect("dirty path must be admitted");
    }
    wait_for_idle(&threshold_coordinator).await;
    assert!(matches!(
        &threshold_executed.lock().await[0].scope,
        ReindexScope::Full
    ));
    assert_eq!(threshold_coordinator.snapshot().await.full_escalations, 1);

    let overflow_executed = Arc::new(Mutex::new(Vec::new()));
    let overflow_coordinator = ReindexCoordinator::new(config(), {
        let overflow_executed = Arc::clone(&overflow_executed);
        move |intent| {
            let overflow_executed = Arc::clone(&overflow_executed);
            async move {
                overflow_executed.lock().await.push(intent.clone());
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("overflow coordinator must be constructible");
    overflow_coordinator
        .admit(
            ReindexSource::Watcher,
            changed("Assets/before-overflow.prefab"),
        )
        .await
        .expect("initial watcher event must queue");
    overflow_coordinator
        .watcher_overflow()
        .await
        .expect("overflow must be accepted");
    wait_for_idle(&overflow_coordinator).await;
    assert!(matches!(
        &overflow_executed.lock().await[0].scope,
        ReindexScope::Full
    ));
    let snapshot = overflow_coordinator.snapshot().await;
    assert_eq!(snapshot.watcher_overflows, 1);
    assert_eq!(snapshot.full_escalations, 1);
}

#[tokio::test]
async fn paths_outside_the_project_are_rejected_before_execution() {
    let builds = Arc::new(AtomicUsize::new(0));
    let coordinator = ReindexCoordinator::new(config(), {
        let builds = Arc::clone(&builds);
        move |intent| {
            builds.fetch_add(1, Ordering::SeqCst);
            async move { Ok(terminal_receipt(&intent)) }
        }
    })
    .expect("coordinator must be constructible");
    let outside = if cfg!(windows) {
        PathBuf::from(r"D:\outside.prefab")
    } else {
        PathBuf::from("/outside.prefab")
    };

    assert!(matches!(
        coordinator
            .admit(ReindexSource::Http, changed(outside))
            .await,
        Err(CoordinatorError::PathOutsideProject { .. })
    ));
    assert_eq!(builds.load(Ordering::SeqCst), 0);
    assert!(coordinator.snapshot().await.is_idle());
}

#[tokio::test]
async fn continuous_events_cannot_postpone_build_past_max_debounce() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let coordinator = ReindexCoordinator::new(
        ReindexCoordinatorConfig::new(project_root())
            .with_debounce(Duration::from_millis(90))
            .with_max_debounce(Duration::from_millis(180))
            .with_max_pending_events(1_000),
        move |intent| {
            let started_tx = started_tx.clone();
            async move {
                let _ignored = started_tx.send(());
                Ok(terminal_receipt(&intent))
            }
        },
    )
    .expect("coordinator must be constructible");

    let producer = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            for _ in 0..20 {
                coordinator
                    .admit(
                        ReindexSource::Watcher,
                        changed("Assets/continuously-changing.prefab"),
                    )
                    .await
                    .expect("continuous event must be accepted");
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
    };

    tokio::time::timeout(Duration::from_millis(500), started_rx.recv())
        .await
        .expect("max debounce must start a build while events are still arriving")
        .expect("executor must report its start");
    assert!(!producer.is_finished());
    producer.await.expect("event producer must complete");
    wait_for_idle(&coordinator).await;
}

#[tokio::test]
async fn filesystem_failures_are_bounded_and_new_admissions_can_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let coordinator = ReindexCoordinator::new(config().with_max_failure_history(2), {
        let attempts = Arc::clone(&attempts);
        move |intent| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 3 {
                    anyhow::bail!("injected filesystem build failure {attempt}");
                }
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");

    for _ in 0..3 {
        coordinator
            .admit(ReindexSource::Http, FilesystemReindexIntent::full())
            .await
            .expect("failed build must still be admitted");
        wait_for_idle(&coordinator).await;
    }
    let failures = coordinator.snapshot().await.failures;
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].sequence, 2);
    assert_eq!(failures[1].sequence, 3);
    assert_eq!(failures[1].scope, ReindexScopeKind::Full);

    coordinator
        .admit(ReindexSource::Http, FilesystemReindexIntent::full())
        .await
        .expect("a new filesystem request must retry after failure");
    wait_for_idle(&coordinator).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    assert_eq!(coordinator.snapshot().await.failures.len(), 2);
}

#[tokio::test]
async fn synchronous_admission_returns_initial_and_terminal_receipts() {
    let coordinator =
        ReindexCoordinator::new(
            config(),
            |intent| async move { Ok(terminal_receipt(&intent)) },
        )
        .expect("coordinator must be constructible");

    let completion = tokio::time::timeout(
        Duration::from_secs(5),
        coordinator.admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::reconcile()),
    )
    .await
    .expect("synchronous admission must complete before the test deadline")
    .expect("synchronous admission must succeed");

    assert_eq!(completion.admission.disposition, ReindexDisposition::Queued);
    assert_eq!(
        completion.terminal,
        terminal_receipt(&ReindexIntent::reconcile())
    );

    let unsupported = coordinator
        .admit_and_wait(
            ReindexSource::Http,
            FilesystemReindexIntent {
                contract_version: SEARCH_GENERATION_CONTRACT_VERSION + 1,
                scope: FilesystemReindexScope::Full,
            },
        )
        .await;
    assert!(matches!(
        unsupported,
        Err(CoordinatorError::UnsupportedContractVersion { .. })
    ));
}

#[tokio::test]
async fn synchronous_admission_reports_executor_errors_and_invalid_receipts() {
    let failed = ReindexCoordinator::new(config(), |_intent| async {
        anyhow::bail!("injected synchronous HTTP build failure")
    })
    .expect("failure coordinator must be constructible");
    let error = failed
        .admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::full())
        .await
        .expect_err("executor failure must reach the waiting caller");
    assert!(matches!(
        error,
        CoordinatorError::ExecutionFailed {
            admission,
            scope: ReindexScopeKind::Full,
            message,
        } if admission.disposition == ReindexDisposition::Queued
            && message.contains("injected synchronous HTTP build failure")
    ));

    let invalid_receipt = ReindexCoordinator::new(config(), |_intent| async {
        Ok(ReindexReceipt {
            contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
            disposition: ReindexDisposition::Queued,
            transaction: None,
            target_revision: None,
            generation: None,
            evidence: Default::default(),
        })
    })
    .expect("receipt validation coordinator must be constructible");
    let error = invalid_receipt
        .admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::reconcile())
        .await
        .expect_err("non-terminal executor receipt must be rejected");
    assert!(matches!(
        error,
        CoordinatorError::ExecutionFailed { message, .. }
            if message.contains("non-terminal disposition")
    ));

    let invalid_generation_version = ReindexCoordinator::new(config(), |intent| async move {
        let digest = DigestV1::hash_bytes(b"invalid nested generation version");
        let mut generation = GenerationStamp::current(
            SearchGenerationId::new(digest),
            WorkspaceId::from_u128(1).expect("nonzero workspace ID"),
            WorkspaceRevision::new(digest),
        );
        generation.contract_version += 1;
        let mut receipt = terminal_receipt(&intent);
        receipt.generation = Some(generation);
        Ok(receipt)
    })
    .expect("nested version coordinator must be constructible");
    let error = invalid_generation_version
        .admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::full())
        .await
        .expect_err("unsupported nested generation version must be rejected");
    assert!(matches!(
        error,
        CoordinatorError::ExecutionFailed { message, .. }
            if message.contains("reindex receipt generation contract version")
    ));
}

#[tokio::test]
async fn synchronous_admission_reports_panics_and_cancelled_executor_tasks() {
    let synchronous_panic = ReindexCoordinator::new(config(), |_intent| {
        panic!("injected synchronous completion panic");
        #[allow(unreachable_code)]
        async {
            Err::<ReindexReceipt, _>(anyhow::anyhow!("unreachable test future"))
        }
    })
    .expect("synchronous panic coordinator must be constructible");
    let error = synchronous_panic
        .admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::full())
        .await
        .expect_err("synchronous executor panic must reach the waiter");
    assert!(matches!(
        error,
        CoordinatorError::ExecutionFailed { message, .. }
            if message.contains("panicked before returning")
    ));

    let asynchronous_panic = ReindexCoordinator::new(config(), |_intent| async move {
        panic!("injected asynchronous completion panic");
        #[allow(unreachable_code)]
        Err::<ReindexReceipt, _>(anyhow::anyhow!("unreachable test future"))
    })
    .expect("asynchronous panic coordinator must be constructible");
    let error = asynchronous_panic
        .admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::reconcile())
        .await
        .expect_err("asynchronous executor panic must reach the waiter");
    assert!(matches!(
        error,
        CoordinatorError::ExecutionFailed { message, .. }
            if message.contains("task") && message.contains("panic")
    ));

    let cancelled = ReindexCoordinator::new(config(), |intent| async move {
        let child = tokio::spawn(std::future::pending::<()>());
        child.abort();
        child.await.map_err(anyhow::Error::new)?;
        Ok(terminal_receipt(&intent))
    })
    .expect("cancelled task coordinator must be constructible");
    let error = cancelled
        .admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::reconcile())
        .await
        .expect_err("cancelled executor child must reach the waiter");
    assert!(matches!(
        error,
        CoordinatorError::ExecutionFailed { message, .. }
            if message.contains("cancel")
    ));
}

#[tokio::test]
async fn coalesced_synchronous_waiters_receive_the_same_terminal_receipt() {
    let builds = Arc::new(AtomicUsize::new(0));
    let coordinator = ReindexCoordinator::new(config(), {
        let builds = Arc::clone(&builds);
        move |intent| {
            let builds = Arc::clone(&builds);
            async move {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");

    let completions = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            coordinator.admit_and_wait(ReindexSource::Http, changed("Assets/first.prefab"),),
            coordinator.admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::reconcile(),),
        )
    })
    .await
    .expect("coalesced waiters must complete before the test deadline");
    let first = completions.0.expect("first waiter must complete");
    let second = completions.1.expect("second waiter must complete");

    let dispositions = [first.admission.disposition, second.admission.disposition];
    assert_eq!(
        dispositions
            .iter()
            .filter(|&&disposition| disposition == ReindexDisposition::Queued)
            .count(),
        1
    );
    assert_eq!(
        dispositions
            .iter()
            .filter(|&&disposition| disposition == ReindexDisposition::Coalesced)
            .count(),
        1
    );
    assert_eq!(first.terminal, second.terminal);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelling_one_coalesced_waiter_does_not_affect_another() {
    let builds = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let coordinator = ReindexCoordinator::new(
        config()
            .with_debounce(Duration::from_millis(500))
            .with_max_debounce(Duration::from_millis(500)),
        {
            let builds = Arc::clone(&builds);
            let release = Arc::clone(&release);
            move |intent| {
                let builds = Arc::clone(&builds);
                let release = Arc::clone(&release);
                async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    let permit = release
                        .acquire_owned()
                        .await
                        .expect("test completion gate must remain open");
                    permit.forget();
                    Ok(terminal_receipt(&intent))
                }
            }
        },
    )
    .expect("coordinator must be constructible");

    let cancelled = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .admit_and_wait(ReindexSource::Http, changed("Assets/cancelled.prefab"))
                .await
        })
    };
    wait_for_http_admissions(&coordinator, 1).await;
    let survivor = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::reconcile())
                .await
        })
    };
    wait_for_http_admissions(&coordinator, 2).await;

    cancelled.abort();
    assert!(
        cancelled
            .await
            .expect_err("cancelled waiter task must not complete")
            .is_cancelled()
    );
    release.add_permits(1);
    let completion = tokio::time::timeout(Duration::from_secs(5), survivor)
        .await
        .expect("surviving waiter must complete before the test deadline")
        .expect("surviving waiter task must not panic")
        .expect("surviving waiter must receive the terminal result");

    assert_eq!(completion.terminal.disposition, ReindexDisposition::Applied);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn synchronous_completion_waiters_are_bounded() {
    let builds = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let coordinator = ReindexCoordinator::new(
        config()
            .with_debounce(Duration::from_millis(500))
            .with_max_debounce(Duration::from_millis(500))
            .with_max_pending_events(2),
        {
            let builds = Arc::clone(&builds);
            let release = Arc::clone(&release);
            move |intent| {
                let builds = Arc::clone(&builds);
                let release = Arc::clone(&release);
                async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    let permit = release
                        .acquire_owned()
                        .await
                        .expect("test completion gate must remain open");
                    permit.forget();
                    Ok(terminal_receipt(&intent))
                }
            }
        },
    )
    .expect("coordinator must be constructible");

    let first = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .admit_and_wait(ReindexSource::Http, changed("Assets/first.prefab"))
                .await
        })
    };
    let second = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .admit_and_wait(ReindexSource::Http, changed("Assets/second.prefab"))
                .await
        })
    };
    wait_for_http_admissions(&coordinator, 2).await;

    let rejected = coordinator
        .admit_and_wait(ReindexSource::Http, FilesystemReindexIntent::reconcile())
        .await;
    assert!(matches!(
        rejected,
        Err(CoordinatorError::CompletionWaiterLimit { maximum: 2 })
    ));

    release.add_permits(1);
    for waiter in [first, second] {
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("accepted waiter must complete before the test deadline")
            .expect("accepted waiter task must not panic")
            .expect("accepted waiter must receive a terminal receipt");
    }
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}
