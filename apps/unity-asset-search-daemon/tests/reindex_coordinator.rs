use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Barrier, Mutex, Semaphore, mpsc};
use unity_asset_search_daemon::coordinator::{
    CoordinatorError, ReindexCoordinator, ReindexCoordinatorConfig, ReindexScopeKind, ReindexSource,
};
use unity_asset_search_index::{
    ReindexDisposition, ReindexIntent, ReindexReceipt, ReindexScope,
    SEARCH_GENERATION_CONTRACT_VERSION,
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

fn changed(path: impl AsRef<Path>) -> ReindexIntent {
    ReindexIntent::changed_paths(vec![path.as_ref().to_path_buf()])
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

fn digest(byte: u8) -> String {
    let encoded_byte = format!("{byte:02x}");
    format!("blake3-v1:{}", encoded_byte.repeat(32))
}

fn change_set_intent(id: u8) -> ReindexIntent {
    change_set_intent_with_payload(id, id)
}

fn change_set_intent_with_payload(transaction: u8, payload: u8) -> ReindexIntent {
    let workspace = "workspace-v1:00000000000000000000000000000001";
    let value = serde_json::json!({
        "contract_version": SEARCH_GENERATION_CONTRACT_VERSION,
        "scope": {
            "kind": "change_set",
            "changes": {
                "version": 1,
                "transaction": digest(transaction),
                "workspace": workspace,
                "from_revision": digest(payload.wrapping_add(64)),
                "to_revision": digest(payload.wrapping_add(128)),
                "changed_sources": [{
                    "version": 1,
                    "workspace": workspace,
                    "kind": "serialized_file",
                    "local": format!("{:032x}", u128::from(payload) + 1),
                }],
                "changed_objects": [],
                "identity_remaps": [],
            },
        },
    });
    serde_json::from_value(value).expect("test ChangeSet must satisfy the public wire contract")
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
async fn four_entry_points_share_one_atomic_admission_and_one_build() {
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
async fn same_transaction_is_coalesced_in_flight_then_reported_as_applied() {
    let builds = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Semaphore::new(0));
    let coordinator = ReindexCoordinator::new(config(), {
        let builds = Arc::clone(&builds);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move |intent| {
            let builds = Arc::clone(&builds);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                builds.fetch_add(1, Ordering::SeqCst);
                wait_at_barrier(&started).await;
                tokio::time::timeout(Duration::from_secs(5), release.acquire())
                    .await
                    .expect("release permit must arrive before the test deadline")
                    .expect("test semaphore must stay open")
                    .forget();
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");
    let intent = change_set_intent(1);

    let first = coordinator
        .admit(ReindexSource::ChangeSet, intent.clone())
        .await
        .expect("first transaction must queue");
    assert_eq!(first.disposition, ReindexDisposition::Queued);
    wait_at_barrier(&started).await;

    let duplicate = coordinator
        .admit(ReindexSource::Http, intent.clone())
        .await
        .expect("duplicate admission must be classified");
    assert_eq!(duplicate.disposition, ReindexDisposition::Coalesced);

    release.add_permits(1);
    wait_for_idle(&coordinator).await;
    let applied = coordinator
        .admit(ReindexSource::ChangeSet, intent)
        .await
        .expect("applied transaction must be classified");
    assert_eq!(applied.disposition, ReindexDisposition::AlreadyApplied);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reused_transaction_with_different_change_set_conflicts_while_queued_and_applied() {
    let builds = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Semaphore::new(0));
    let blocker = change_set_intent(41);
    let blocker_transaction = blocker.scope.transaction();
    let coordinator = ReindexCoordinator::new(config(), {
        let builds = Arc::clone(&builds);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move |intent| {
            let builds = Arc::clone(&builds);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                builds.fetch_add(1, Ordering::SeqCst);
                if intent.scope.transaction() == blocker_transaction {
                    wait_at_barrier(&started).await;
                    tokio::time::timeout(Duration::from_secs(5), release.acquire())
                        .await
                        .expect("release permit must arrive before the test deadline")
                        .expect("test semaphore must stay open")
                        .forget();
                }
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");
    let original = change_set_intent_with_payload(42, 1);
    let conflicting = change_set_intent_with_payload(42, 2);

    coordinator
        .admit(ReindexSource::ChangeSet, blocker)
        .await
        .expect("blocking transaction must queue");
    wait_at_barrier(&started).await;
    coordinator
        .admit(ReindexSource::ChangeSet, original)
        .await
        .expect("tested transaction must remain queued behind the blocker");

    let tracked_conflict = coordinator
        .admit(ReindexSource::ChangeSet, conflicting.clone())
        .await;
    assert!(matches!(
        tracked_conflict,
        Err(CoordinatorError::TransactionConflict {
            transaction,
            existing_change_set,
            incoming_change_set,
        }) if transaction.to_string() == digest(42)
            && existing_change_set != incoming_change_set
    ));
    assert_eq!(coordinator.snapshot().await.pending_transactions, 1);

    release.add_permits(1);
    wait_for_idle(&coordinator).await;

    let applied_conflict = coordinator
        .admit(ReindexSource::ChangeSet, conflicting)
        .await;
    assert!(matches!(
        applied_conflict,
        Err(CoordinatorError::TransactionConflict {
            transaction,
            existing_change_set,
            incoming_change_set,
        }) if transaction.to_string() == digest(42)
            && existing_change_set != incoming_change_set
    ));
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    let snapshot = coordinator.snapshot().await;
    assert_eq!(snapshot.tracked_transactions, 0);
    assert_eq!(snapshot.applied_transactions, 2);
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
            .admit(ReindexSource::Timer, ReindexIntent::reconcile())
            .await
            .expect("reconcile must merge")
            .disposition,
        ReindexDisposition::Coalesced
    );
    assert_eq!(
        coordinator
            .admit(ReindexSource::Http, ReindexIntent::full())
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
async fn failed_transaction_is_not_marked_applied_and_can_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let coordinator = ReindexCoordinator::new(config(), {
        let attempts = Arc::clone(&attempts);
        move |intent| {
            let attempts = Arc::clone(&attempts);
            async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("injected build failure");
                }
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");
    let intent = change_set_intent(2);

    coordinator
        .admit(ReindexSource::ChangeSet, intent.clone())
        .await
        .expect("first attempt must queue");
    wait_for_idle(&coordinator).await;
    let failed = coordinator.snapshot().await;
    assert_eq!(failed.applied_transactions, 0);
    assert_eq!(failed.tracked_transactions, 0);
    assert_eq!(failed.failures.len(), 1);
    assert!(
        failed.failures[0]
            .message
            .contains("injected build failure")
    );

    let retry = coordinator
        .admit(ReindexSource::ChangeSet, intent.clone())
        .await
        .expect("failed transaction must be retryable");
    assert_eq!(retry.disposition, ReindexDisposition::Queued);
    wait_for_idle(&coordinator).await;

    let duplicate = coordinator
        .admit(ReindexSource::ChangeSet, intent)
        .await
        .expect("successful retry must be remembered");
    assert_eq!(duplicate.disposition, ReindexDisposition::AlreadyApplied);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn synchronous_executor_panic_releases_transaction_for_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let coordinator = ReindexCoordinator::new(config(), {
        let attempts = Arc::clone(&attempts);
        move |intent| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("injected synchronous executor panic");
            }
            async move { Ok(terminal_receipt(&intent)) }
        }
    })
    .expect("coordinator must be constructible");
    let intent = change_set_intent(3);

    coordinator
        .admit(ReindexSource::ChangeSet, intent.clone())
        .await
        .expect("panicking transaction must first queue");
    wait_for_idle(&coordinator).await;
    let failed = coordinator.snapshot().await;
    assert_eq!(failed.tracked_transactions, 0);
    assert_eq!(failed.applied_transactions, 0);
    assert!(
        failed.failures[0]
            .message
            .contains("panicked before returning")
    );

    coordinator
        .admit(ReindexSource::ChangeSet, intent)
        .await
        .expect("transaction must be retryable after synchronous panic");
    wait_for_idle(&coordinator).await;
    assert_eq!(coordinator.snapshot().await.applied_transactions, 1);
}

#[tokio::test]
async fn asynchronous_executor_panic_releases_transaction_and_wakes_idle_waiters() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let coordinator = ReindexCoordinator::new(config(), {
        let attempts = Arc::clone(&attempts);
        move |intent| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    panic!("injected asynchronous executor panic");
                }
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");
    let intent = change_set_intent(4);

    coordinator
        .admit(ReindexSource::ChangeSet, intent.clone())
        .await
        .expect("panicking transaction must first queue");
    wait_for_idle(&coordinator).await;
    let failed = coordinator.snapshot().await;
    assert_eq!(failed.tracked_transactions, 0);
    assert_eq!(failed.applied_transactions, 0);
    assert!(
        failed.failures[0].message.contains("task") && failed.failures[0].message.contains("panic")
    );

    coordinator
        .admit(ReindexSource::ChangeSet, intent)
        .await
        .expect("transaction must be retryable after asynchronous panic");
    wait_for_idle(&coordinator).await;
    assert_eq!(coordinator.snapshot().await.applied_transactions, 1);
}

#[tokio::test]
async fn cancelled_executor_child_releases_transaction_and_wakes_idle_waiters() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let coordinator = ReindexCoordinator::new(config(), {
        let attempts = Arc::clone(&attempts);
        move |intent| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    let child = tokio::spawn(std::future::pending::<()>());
                    child.abort();
                    match child.await {
                        Err(error) if error.is_cancelled() => {
                            anyhow::bail!("injected cancelled build child")
                        }
                        Ok(()) => anyhow::bail!("cancelled build child unexpectedly completed"),
                        Err(error) => {
                            anyhow::bail!("build child failed without cancellation: {error}")
                        }
                    }
                }
                Ok(terminal_receipt(&intent))
            }
        }
    })
    .expect("coordinator must be constructible");
    let intent = change_set_intent(5);

    coordinator
        .admit(ReindexSource::ChangeSet, intent.clone())
        .await
        .expect("cancelled transaction must first queue");
    wait_for_idle(&coordinator).await;
    let failed = coordinator.snapshot().await;
    assert_eq!(failed.tracked_transactions, 0);
    assert_eq!(failed.applied_transactions, 0);
    assert!(failed.failures[0].message.contains("cancelled build child"));

    coordinator
        .admit(ReindexSource::ChangeSet, intent)
        .await
        .expect("transaction must be retryable after cancellation");
    wait_for_idle(&coordinator).await;
    assert_eq!(coordinator.snapshot().await.applied_transactions, 1);
}

#[tokio::test]
async fn dirty_path_event_and_watcher_overflow_limits_upgrade_to_full() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let coordinator = ReindexCoordinator::new(
        config().with_max_dirty_paths(2).with_max_pending_events(8),
        {
            let executed = Arc::clone(&executed);
            move |intent| {
                let executed = Arc::clone(&executed);
                async move {
                    executed.lock().await.push(intent.clone());
                    Ok(terminal_receipt(&intent))
                }
            }
        },
    )
    .expect("coordinator must be constructible");

    for path in ["Assets/c.prefab", "Assets/a.prefab", "Assets/b.prefab"] {
        coordinator
            .admit(ReindexSource::Watcher, changed(path))
            .await
            .expect("watcher path must be accepted");
    }
    wait_for_idle(&coordinator).await;
    assert!(matches!(
        &executed.lock().await[0].scope,
        ReindexScope::Full
    ));
    assert_eq!(coordinator.snapshot().await.full_escalations, 1);

    let event_executed = Arc::new(Mutex::new(Vec::new()));
    let event_coordinator = ReindexCoordinator::new(
        config().with_max_dirty_paths(8).with_max_pending_events(2),
        {
            let event_executed = Arc::clone(&event_executed);
            move |intent| {
                let event_executed = Arc::clone(&event_executed);
                async move {
                    event_executed.lock().await.push(intent.clone());
                    Ok(terminal_receipt(&intent))
                }
            }
        },
    )
    .expect("event coordinator must be constructible");
    for _ in 0..3 {
        event_coordinator
            .admit(ReindexSource::Watcher, changed("Assets/repeated.prefab"))
            .await
            .expect("watcher event must be accepted");
    }
    wait_for_idle(&event_coordinator).await;
    assert!(matches!(
        &event_executed.lock().await[0].scope,
        ReindexScope::Full
    ));

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
async fn transaction_queue_and_histories_remain_bounded() {
    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Semaphore::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let overlap = Arc::new(AtomicBool::new(false));
    let coordinator = ReindexCoordinator::new(
        config()
            .with_max_pending_transactions(1)
            .with_max_transaction_history(2)
            .with_max_failure_history(2),
        {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let active = Arc::clone(&active);
            let overlap = Arc::clone(&overlap);
            move |intent| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let active = Arc::clone(&active);
                let overlap = Arc::clone(&overlap);
                async move {
                    if active.fetch_add(1, Ordering::SeqCst) != 0 {
                        overlap.store(true, Ordering::SeqCst);
                    }
                    if intent.scope.transaction() == change_set_intent(10).scope.transaction() {
                        wait_at_barrier(&started).await;
                        tokio::time::timeout(Duration::from_secs(5), release.acquire())
                            .await
                            .expect("release permit must arrive before the test deadline")
                            .expect("test semaphore must stay open")
                            .forget();
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(terminal_receipt(&intent))
                }
            }
        },
    )
    .expect("coordinator must be constructible");

    coordinator
        .admit(ReindexSource::ChangeSet, change_set_intent(10))
        .await
        .expect("first transaction must queue");
    wait_at_barrier(&started).await;
    coordinator
        .admit(ReindexSource::ChangeSet, change_set_intent(11))
        .await
        .expect("one queued transaction must fit");
    assert_eq!(active.load(Ordering::SeqCst), 1);
    assert!(matches!(
        coordinator
            .admit(ReindexSource::ChangeSet, change_set_intent(12))
            .await,
        Err(CoordinatorError::TransactionQueueFull { maximum: 1 })
    ));
    release.add_permits(1);
    wait_for_idle(&coordinator).await;

    coordinator
        .admit(ReindexSource::ChangeSet, change_set_intent(12))
        .await
        .expect("transaction must fit after drain");
    wait_for_idle(&coordinator).await;
    assert_eq!(coordinator.snapshot().await.applied_transactions, 2);
    assert!(!overlap.load(Ordering::SeqCst));

    let failure_coordinator =
        ReindexCoordinator::new(config().with_max_failure_history(2), |_intent| async {
            anyhow::bail!("bounded failure")
        })
        .expect("failure coordinator must be constructible");
    for _ in 0..3 {
        failure_coordinator
            .admit(ReindexSource::Http, ReindexIntent::full())
            .await
            .expect("failed full build must still be admitted");
        wait_for_idle(&failure_coordinator).await;
    }
    let failures = failure_coordinator.snapshot().await.failures;
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].sequence, 2);
    assert_eq!(failures[1].sequence, 3);
}

#[tokio::test]
async fn synchronous_admission_returns_initial_and_terminal_receipts() {
    let coordinator =
        ReindexCoordinator::new(
            config(),
            |intent| async move { Ok(terminal_receipt(&intent)) },
        )
        .expect("coordinator must be constructible");
    let intent = ReindexIntent::reconcile();

    let completion = tokio::time::timeout(
        Duration::from_secs(5),
        coordinator.admit_and_wait(ReindexSource::Http, intent.clone()),
    )
    .await
    .expect("synchronous admission must complete before the test deadline")
    .expect("synchronous admission must succeed");

    assert_eq!(completion.admission.disposition, ReindexDisposition::Queued);
    assert_eq!(completion.terminal, terminal_receipt(&intent));

    let unsupported = coordinator
        .admit_and_wait(ReindexSource::Http, change_set_intent(91))
        .await;
    assert!(matches!(
        unsupported,
        Err(CoordinatorError::SynchronousChangeSetUnsupported)
    ));
}

#[tokio::test]
async fn synchronous_admission_reports_executor_errors_directly() {
    let coordinator = ReindexCoordinator::new(config(), |_intent| async {
        anyhow::bail!("injected synchronous HTTP build failure")
    })
    .expect("coordinator must be constructible");

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        coordinator.admit_and_wait(ReindexSource::Http, ReindexIntent::full()),
    )
    .await
    .expect("failed synchronous admission must complete before the test deadline")
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
        .admit_and_wait(ReindexSource::Http, ReindexIntent::full())
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
        .admit_and_wait(ReindexSource::Http, ReindexIntent::reconcile())
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
        .admit_and_wait(ReindexSource::Http, ReindexIntent::reconcile())
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
            coordinator.admit_and_wait(ReindexSource::Http, changed("Assets/first.prefab")),
            coordinator.admit_and_wait(ReindexSource::Http, ReindexIntent::reconcile()),
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
                .admit_and_wait(ReindexSource::Http, ReindexIntent::reconcile())
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
    release.add_permits(2);
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
        .admit_and_wait(ReindexSource::Http, ReindexIntent::reconcile())
        .await;
    assert!(matches!(
        rejected,
        Err(CoordinatorError::CompletionWaiterLimit { maximum: 2 })
    ));

    release.add_permits(2);
    for waiter in [first, second] {
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("accepted waiter must complete before the test deadline")
            .expect("accepted waiter task must not panic")
            .expect("accepted waiter must receive a terminal receipt");
    }
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}
