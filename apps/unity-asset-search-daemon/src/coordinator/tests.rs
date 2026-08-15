use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::{
    CoordinatorError, ReindexCompletion, ReindexCoordinator, ReindexCoordinatorConfig,
    ReindexCoordinatorRuntime, ReindexExecution, ReindexObservation, ReindexObservationProgress,
    ReindexScopeKind, ReindexSource,
};
use tokio::sync::{Mutex, Semaphore};
use unity_asset_search_index::{
    FilesystemReindexIntent, FilesystemReindexScope, IndexPaths, ProjectPathSpace,
};
use unity_asset_search_protocol::{
    GenerationStatus, PortablePath, QueryPolicyId, ReindexDisposition, ReindexReceipt,
    SEARCH_PROTOCOL_REVISION, SearchCapabilities, StatusResponse,
};

use crate::lifecycle::{AdmissionGate, AdmissionLifecycle, DaemonTaskKind};

struct ProjectFixture {
    _temporary: tempfile::TempDir,
    paths: IndexPaths,
}

impl ProjectFixture {
    fn new() -> Self {
        let temporary = crate::secure_test_tempdir();
        let assets = temporary.path().join("Assets");
        std::fs::create_dir(&assets).expect("create coordinator Assets root");
        let index_root = temporary.path().join(".unity-asset-index");
        let paths = IndexPaths::for_project(
            temporary.path().to_path_buf(),
            Some(index_root),
            Some(vec![assets]),
        )
        .expect("create coordinator project path space");
        Self {
            _temporary: temporary,
            paths,
        }
    }

    fn path_space(&self) -> &ProjectPathSpace {
        self.paths.project_path_space()
    }
}

fn project_paths() -> ProjectPathSpace {
    let project = crate::secure_test_tempdir();
    let assets = project.path().join("Assets");
    std::fs::create_dir(&assets).expect("create coordinator Assets root");
    let index_root = project.path().join(".unity-asset-index");
    let paths = IndexPaths::for_project(
        project.path().to_path_buf(),
        Some(index_root),
        Some(vec![assets]),
    )
    .expect("create coordinator project path space");
    paths.project_path_space().clone()
}

fn config() -> ReindexCoordinatorConfig {
    config_for(project_paths())
}

fn config_for(project_paths: ProjectPathSpace) -> ReindexCoordinatorConfig {
    ReindexCoordinatorConfig::new(project_paths)
        .with_debounce(Duration::from_millis(20))
        .with_max_debounce(Duration::from_millis(100))
}

fn receipt(disposition: ReindexDisposition) -> ReindexReceipt {
    ReindexReceipt {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        disposition,
        transaction: None,
        target_revision: None,
        generation: None,
        evidence: Default::default(),
    }
}

fn status_for(receipt: &ReindexReceipt) -> StatusResponse {
    let generation = GenerationStatus {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        active: receipt.generation.clone(),
        building_revision: None,
        last_failure: None,
    };
    StatusResponse {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        daemon: unity_asset_search_protocol::DaemonLifecycleStatus::unmanaged(&generation, false),
        generation,
        query_policy_id: QueryPolicyId::from_bytes([7; 32]),
        capabilities: SearchCapabilities::current(),
        project_root: PortablePath::new("/unity-asset-coordinator-tests").unwrap(),
        generation_root: PortablePath::new("/unity-asset-coordinator-tests/index").unwrap(),
        scan_roots: vec![PortablePath::new("/unity-asset-coordinator-tests/Assets").unwrap()],
        indexed_assets: 0,
        indexed_search_documents: 0,
        indexed_reference_facts: 0,
        incomplete_assets: 0,
        projection_truncations: 0,
        last_build_duration_ms: Some(1),
        last_build_unix_ms: Some(1),
        indexing: false,
    }
}

fn execution(intent: &FilesystemReindexIntent) -> ReindexExecution {
    let receipt = receipt(ReindexDisposition::Applied);
    let status = status_for(&receipt);
    let _ = intent;
    ReindexExecution::new(receipt, status)
}

fn changed(project_paths: &ProjectPathSpace, path: impl AsRef<Path>) -> FilesystemReindexIntent {
    FilesystemReindexIntent::changed_paths(
        project_paths
            .resolve_set([path])
            .expect("resolve coordinator changed path"),
    )
}

async fn wait_for_idle(coordinator: &ReindexCoordinator) {
    tokio::time::timeout(Duration::from_secs(5), coordinator.wait_for_idle())
        .await
        .expect("coordinator must become idle before the deadline");
}

async fn wait_for_completion(
    mut observation: ReindexObservation,
) -> Result<ReindexCompletion, CoordinatorError> {
    loop {
        match observation.next_progress().await {
            ReindexObservationProgress::Coalesced | ReindexObservationProgress::Running => {}
            ReindexObservationProgress::Cancelled => {
                panic!("test observation was unexpectedly cancelled")
            }
            ReindexObservationProgress::Terminal(result) => return *result,
        }
    }
}

#[test]
fn invalid_configuration_is_rejected_before_runner_start() {
    let invalid = ReindexCoordinatorRuntime::start(
        ReindexCoordinatorConfig::new(project_paths()).with_debounce(Duration::ZERO),
        |intent| async move { Ok(execution(&intent)) },
    );
    assert!(matches!(
        invalid,
        Err(CoordinatorError::InvalidConfiguration(_))
    ));
}

#[tokio::test]
async fn all_boundaries_share_one_serial_coalescing_window() {
    let project_paths = project_paths();
    let builds = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let overlapped = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _runtime = ReindexCoordinatorRuntime::start(config_for(project_paths.clone()), {
        let builds = Arc::clone(&builds);
        let active = Arc::clone(&active);
        let overlapped = Arc::clone(&overlapped);
        let seen = Arc::clone(&seen);
        move |intent| {
            let builds = Arc::clone(&builds);
            let active = Arc::clone(&active);
            let overlapped = Arc::clone(&overlapped);
            let seen = Arc::clone(&seen);
            async move {
                builds.fetch_add(1, Ordering::SeqCst);
                if active.fetch_add(1, Ordering::SeqCst) != 0 {
                    overlapped.store(true, Ordering::SeqCst);
                }
                seen.lock().await.push(intent.clone());
                tokio::time::sleep(Duration::from_millis(10)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = _runtime.coordinator();

    for (source, path) in [
        (ReindexSource::Startup, "Assets/start.prefab"),
        (ReindexSource::Watcher, "Assets/watch.prefab"),
        (ReindexSource::Timer, "Assets/timer.prefab"),
        (
            ReindexSource::SemanticUpgrade,
            "Assets/semantic-upgrade.prefab",
        ),
        (ReindexSource::Client, "Assets/client.prefab"),
    ] {
        coordinator
            .admit(source, changed(&project_paths, path))
            .await
            .unwrap();
    }
    wait_for_idle(&coordinator).await;

    assert!(!overlapped.load(Ordering::SeqCst));
    assert!(builds.load(Ordering::SeqCst) <= 2);
    let snapshot = coordinator.snapshot().await;
    assert_eq!(snapshot.admissions.startup, 1);
    assert_eq!(snapshot.admissions.watcher, 1);
    assert_eq!(snapshot.admissions.timer, 1);
    assert_eq!(snapshot.admissions.semantic_upgrade, 1);
    assert_eq!(snapshot.admissions.client, 1);
    assert!(!seen.lock().await.is_empty());
}

#[tokio::test]
async fn changed_paths_are_normalized_and_cross_project_paths_fail_closed() {
    let owned_project = ProjectFixture::new();
    let owned_project_paths = owned_project.path_space();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let _runtime = ReindexCoordinatorRuntime::start(config_for(owned_project_paths.clone()), {
        let observed = Arc::clone(&observed);
        move |intent| {
            let observed = Arc::clone(&observed);
            async move {
                observed.lock().await.push(intent.clone());
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = _runtime.coordinator();
    coordinator
        .admit(
            ReindexSource::Client,
            changed(owned_project_paths, "Assets/../Assets/hero.prefab"),
        )
        .await
        .unwrap();
    wait_for_idle(&coordinator).await;
    let intents = observed.lock().await;
    let FilesystemReindexScope::ChangedPaths { paths } = &intents[0].scope else {
        panic!("changed path admission must remain incremental");
    };
    assert_eq!(
        paths
            .iter()
            .map(|path| path.as_relative_path())
            .collect::<Vec<_>>(),
        [Path::new("Assets/hero.prefab")]
    );
    drop(intents);

    let foreign_project = ProjectFixture::new();
    let foreign_paths = foreign_project.path_space();
    let owned_project_id = owned_project_paths.project_id();
    let foreign_project_id = foreign_paths.project_id();
    assert_ne!(owned_project_id, foreign_project_id);
    let foreign = changed(foreign_paths, "Assets/foreign.prefab");
    match coordinator.admit(ReindexSource::Client, foreign).await {
        Err(CoordinatorError::ChangedPathProjectMismatch { expected, actual }) => {
            assert_eq!(expected, owned_project_id);
            assert_eq!(actual, foreign_project_id);
        }
        unexpected => panic!("foreign changed paths were not rejected: {unexpected:?}"),
    }
}

#[tokio::test]
async fn watcher_overflow_escalates_to_a_full_scan() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let _runtime = ReindexCoordinatorRuntime::start(config(), {
        let observed = Arc::clone(&observed);
        move |intent| {
            let observed = Arc::clone(&observed);
            async move {
                observed.lock().await.push(intent.clone());
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = _runtime.coordinator();
    let admission = coordinator.watcher_overflow().await.unwrap();
    assert!(matches!(
        admission.disposition,
        ReindexDisposition::Queued | ReindexDisposition::Coalesced
    ));
    wait_for_idle(&coordinator).await;
    assert!(matches!(
        observed.lock().await[0].scope,
        FilesystemReindexScope::Full
    ));
    assert_eq!(coordinator.snapshot().await.watcher_overflows, 1);
}

#[tokio::test]
async fn observed_admission_survives_the_requesting_connection() {
    let _runtime =
        ReindexCoordinatorRuntime::start(config(), |intent| async move { Ok(execution(&intent)) })
            .unwrap();
    let coordinator = _runtime.coordinator();
    let observation = coordinator
        .admit_observed(ReindexSource::Client, FilesystemReindexIntent::reconcile())
        .await
        .unwrap();
    assert!(matches!(
        observation.admission().disposition,
        ReindexDisposition::Queued | ReindexDisposition::Coalesced
    ));
    let completed = wait_for_completion(observation).await.unwrap();
    assert_eq!(completed.terminal.disposition, ReindexDisposition::Applied);
    assert!(!completed.status.indexing);
}

#[tokio::test]
async fn dropping_a_waiter_never_cancels_admitted_work() {
    let gate = Arc::new(Semaphore::new(0));
    let builds = Arc::new(AtomicUsize::new(0));
    let _runtime = ReindexCoordinatorRuntime::start(config(), {
        let gate = Arc::clone(&gate);
        let builds = Arc::clone(&builds);
        move |intent| {
            let gate = Arc::clone(&gate);
            let builds = Arc::clone(&builds);
            async move {
                let permit = gate.acquire().await.unwrap();
                permit.forget();
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = _runtime.coordinator();
    let observation = coordinator
        .admit_observed(ReindexSource::Client, FilesystemReindexIntent::full())
        .await
        .unwrap();
    drop(observation);
    gate.add_permits(1);
    wait_for_idle(&coordinator).await;
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failures_are_bounded_and_the_runner_recovers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let _runtime = ReindexCoordinatorRuntime::start(config().with_max_failure_history(1), {
        let calls = Arc::clone(&calls);
        move |intent| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if call == 0 {
                    anyhow::bail!("injected failure")
                }
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = _runtime.coordinator();
    coordinator
        .admit(ReindexSource::Client, FilesystemReindexIntent::full())
        .await
        .unwrap();
    wait_for_idle(&coordinator).await;
    assert!(coordinator.snapshot().await.last_completion_failed);
    coordinator
        .admit(ReindexSource::Client, FilesystemReindexIntent::reconcile())
        .await
        .unwrap();
    wait_for_idle(&coordinator).await;
    let snapshot = coordinator.snapshot().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(!snapshot.last_completion_failed);
    assert_eq!(snapshot.failures.len(), 1);
    assert_eq!(snapshot.failures[0].scope, ReindexScopeKind::Full);
}

#[tokio::test]
async fn completion_waiters_are_explicitly_bounded() {
    let gate = Arc::new(Semaphore::new(0));
    let _runtime = ReindexCoordinatorRuntime::start(config().with_max_pending_events(1), {
        let gate = Arc::clone(&gate);
        move |intent| {
            let gate = Arc::clone(&gate);
            async move {
                let permit = gate.acquire().await.unwrap();
                permit.forget();
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = _runtime.coordinator();
    let first = coordinator
        .admit_observed(ReindexSource::Client, FilesystemReindexIntent::full())
        .await
        .unwrap();
    assert!(matches!(
        coordinator
            .admit_observed(ReindexSource::Client, FilesystemReindexIntent::reconcile())
            .await,
        Err(CoordinatorError::CompletionWaiterLimit { maximum: 1 })
    ));
    gate.add_permits(1);
    wait_for_completion(first).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_closes_admission_and_joins_every_accepted_build() {
    let builds = Arc::new(AtomicUsize::new(0));
    let mut runtime = ReindexCoordinatorRuntime::start(config(), {
        let builds = Arc::clone(&builds);
        move |intent| {
            let builds = Arc::clone(&builds);
            async move {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = runtime.coordinator();
    coordinator
        .admit(ReindexSource::Client, FilesystemReindexIntent::full())
        .await
        .unwrap();

    runtime.shutdown().await.unwrap();

    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert!(matches!(
        coordinator
            .admit(ReindexSource::Watcher, FilesystemReindexIntent::reconcile())
            .await,
        Err(CoordinatorError::ShuttingDown)
    ));
}

#[tokio::test(start_paused = true)]
async fn cancelled_shutdown_join_can_resume_without_detaching_runner() {
    let started = Arc::new(Semaphore::new(0));
    let finish = Arc::new(Semaphore::new(0));
    let mut runtime = ReindexCoordinatorRuntime::start(config(), {
        let started = Arc::clone(&started);
        let finish = Arc::clone(&finish);
        move |intent| {
            let started = Arc::clone(&started);
            let finish = Arc::clone(&finish);
            async move {
                started.add_permits(1);
                let permit = finish.acquire().await.unwrap();
                permit.forget();
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = runtime.coordinator();
    coordinator
        .admit(ReindexSource::Client, FilesystemReindexIntent::full())
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    let permit = started.acquire().await.unwrap();
    permit.forget();

    let mut shutdown = Box::pin(runtime.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut shutdown)
            .await
            .is_err()
    );
    finish.add_permits(1);
    shutdown.await.unwrap();
}

#[tokio::test]
async fn coordinator_client_does_not_retain_executor_resources_after_join() {
    let executor_resource = Arc::new(());
    let weak_resource = Arc::downgrade(&executor_resource);
    let mut runtime = ReindexCoordinatorRuntime::start(config(), {
        let executor_resource = Arc::clone(&executor_resource);
        move |intent| {
            let executor_resource = Arc::clone(&executor_resource);
            async move {
                let _retain_for_execution = executor_resource;
                Ok(execution(&intent))
            }
        }
    })
    .unwrap();
    let coordinator = runtime.coordinator();
    drop(executor_resource);

    runtime.shutdown().await.unwrap();
    drop(runtime);

    assert!(weak_resource.upgrade().is_none());
    assert!(matches!(
        coordinator
            .admit(ReindexSource::Client, FilesystemReindexIntent::full())
            .await,
        Err(CoordinatorError::ShuttingDown)
    ));
}

#[tokio::test]
async fn runner_panic_closes_lifecycle_and_fails_every_pending_observer() {
    let lifecycle = AdmissionGate::default();
    let mut runtime = ReindexCoordinatorRuntime::start_supervised(
        config()
            .with_debounce(Duration::from_secs(60))
            .with_max_debounce(Duration::from_secs(60)),
        lifecycle.clone(),
        |intent| async move { Ok(execution(&intent)) },
    )
    .unwrap();
    let coordinator = runtime.coordinator();
    let observation = coordinator
        .admit_observed(ReindexSource::Client, FilesystemReindexIntent::reconcile())
        .await
        .unwrap();

    coordinator.panic_runner();

    let failure = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let AdmissionLifecycle::Failed(failure) = lifecycle.lifecycle() {
                break failure;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runner panic must close lifecycle admission");
    assert_eq!(failure.task, DaemonTaskKind::ReindexCoordinator);
    assert!(
        failure
            .message
            .contains("test-injected coordinator runner panic")
    );

    let observation_failure = wait_for_completion(observation).await.unwrap_err();
    assert!(matches!(
        observation_failure,
        CoordinatorError::RunnerTerminated { ref message }
            if message.contains("test-injected coordinator runner panic")
    ));
    assert!(matches!(
        coordinator
            .admit(ReindexSource::Client, FilesystemReindexIntent::full())
            .await,
        Err(CoordinatorError::RunnerTerminated { message })
            if message.contains("test-injected coordinator runner panic")
    ));
    let snapshot = coordinator.snapshot().await;
    assert!(!snapshot.running);
    assert!(snapshot.in_flight.is_none());
    assert!(snapshot.pending_general.is_none());
    assert!(snapshot.runtime_failure.is_some());

    assert!(matches!(
        runtime.shutdown().await,
        Err(CoordinatorError::RunnerTerminated { message })
            if message.contains("test-injected coordinator runner panic")
    ));
}
