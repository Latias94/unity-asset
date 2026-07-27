use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use notify::Watcher as _;
use tokio::sync::mpsc;

use unity_asset_search_daemon::app::router as daemon_router;
use unity_asset_search_daemon::coordinator::{
    ReindexCoordinator, ReindexCoordinatorConfig, ReindexSource,
};
use unity_asset_search_daemon::security::{TokenStore, validate_listen_addr};
use unity_asset_search_index::{
    AssetLoadBudget, FilesystemReindexIntent, IndexPaths, SearchIndex, SearchIndexOptions,
};

const WATCH_CHANNEL_CAPACITY: usize = 1_024;

#[derive(Debug, Parser)]
#[command(name = "unity-asset-search-daemon")]
struct Args {
    #[arg(long)]
    project_root: PathBuf,

    #[arg(long)]
    index_dir: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    scan_root: Vec<PathBuf>,

    #[arg(long)]
    scan_all: bool,

    #[arg(long, default_value = "127.0.0.1:9781")]
    listen: SocketAddr,

    /// Rotate the persisted bearer token before accepting requests.
    #[arg(long)]
    rotate_token: bool,

    #[arg(long)]
    no_auto_reindex: bool,

    #[arg(long)]
    watch: bool,

    #[arg(long, default_value_t = 1500)]
    watch_debounce_ms: u64,

    /// Maximum dirty paths retained by the coordinator before escalating to a full reindex.
    ///
    /// Set to 0 to disable this threshold.
    #[arg(long, default_value_t = 5000)]
    watch_full_scan_threshold: usize,

    /// Periodically reconcile the full project to recover from missed watcher events.
    ///
    /// Set to 0 to disable.
    #[arg(long, default_value_t = 0)]
    watch_reconcile_interval_ms: u64,

    /// Also index AssetBundle `m_Container` asset paths.
    #[arg(long)]
    index_bundle_container_entries: bool,

    /// Enable bundle-container indexing and ignore the project-root `.gitignore`.
    #[arg(long, alias = "everything")]
    search_everything: bool,

    #[arg(long, hide = true)]
    unityflow: bool,

    /// Cap indexed container entries per bundle.
    #[arg(long, default_value_t = 50_000)]
    max_bundle_container_entries_per_bundle: usize,

    /// Do not apply project-root ignore files while scanning.
    #[arg(long)]
    no_ignore_files: bool,

    /// Ignore the project-root `.gitignore` while honoring the two tool ignore files.
    #[arg(long)]
    no_gitignore: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    validate_listen_addr(args.listen)?;

    let preset_everything = args.search_everything || args.unityflow;
    let scan_roots = if args.scan_all {
        Some(vec![PathBuf::from(".")])
    } else if args.scan_root.is_empty() {
        None
    } else {
        Some(args.scan_root.clone())
    };
    let paths = IndexPaths::for_project(
        args.project_root.clone(),
        args.index_dir.clone(),
        scan_roots,
    )?;
    let options = SearchIndexOptions {
        index_bundle_container_entries: preset_everything || args.index_bundle_container_entries,
        max_bundle_container_entries_per_bundle: args.max_bundle_container_entries_per_bundle,
        respect_project_root_ignore_files: !args.no_ignore_files,
        respect_project_root_gitignore: !(preset_everything || args.no_gitignore),
        ..SearchIndexOptions::default()
    };

    let mut open_budget = AssetLoadBudget::default();
    let index = SearchIndex::open_or_create_with_options(paths.clone(), options, &mut open_budget)?;
    let token_store = TokenStore::open(paths.index_root())?;
    let _daemon_lease = token_store.acquire_daemon_lease()?;
    let token = if args.rotate_token {
        let rotation = token_store.create_or_rotate()?;
        if let Some(warning) = rotation.warning() {
            eprintln!("daemon token startup rotation warning: {warning}");
        }
        rotation.into_token()
    } else {
        token_store.load_or_create()?
    };

    let build_index = index.clone();
    let coordinator_config = coordinator_config(&args, paths.project_root());
    let coordinator = ReindexCoordinator::new(coordinator_config, move |intent| {
        let index = build_index.clone();
        async move {
            let result = tokio::task::spawn_blocking(move || {
                let mut budget = AssetLoadBudget::default();
                index.reindex(intent, &mut budget)
            })
            .await
            .map_err(|_| anyhow::anyhow!("reindex worker terminated unexpectedly"))?;
            result.map_err(anyhow::Error::new)
        }
    })?;

    if !args.no_auto_reindex {
        coordinator
            .admit(ReindexSource::Startup, FilesystemReindexIntent::reconcile())
            .await?;
    }

    if args.watch {
        let watcher_coordinator = coordinator.clone();
        let scan_roots = paths.scan_roots().to_vec();
        let project_root = paths.project_root().to_path_buf();
        let index_root = paths.index_root().to_path_buf();
        let _watcher_task = tokio::spawn(async move {
            if let Err(error) =
                watch_and_reindex(watcher_coordinator, scan_roots, project_root, index_root).await
            {
                eprintln!("search watcher stopped: {error}");
            }
        });
    }

    if args.watch && args.watch_reconcile_interval_ms > 0 {
        let timer_coordinator = coordinator.clone();
        let interval = Duration::from_millis(args.watch_reconcile_interval_ms);
        let _reconcile_task = tokio::spawn(async move {
            reconcile_loop(timer_coordinator, interval).await;
        });
    }

    let app = daemon_router(index, coordinator, token_store, token);

    eprintln!(
        "unity-asset-search-daemon listening on {} (index: {})",
        args.listen,
        paths.index_root().display()
    );
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn coordinator_config(args: &Args, project_root: &Path) -> ReindexCoordinatorConfig {
    let debounce = Duration::from_millis(args.watch_debounce_ms.max(100));
    let max_debounce = Duration::from_millis(args.watch_debounce_ms.max(100).saturating_mul(4));
    let maximum_dirty_paths = if args.watch_full_scan_threshold == 0 {
        usize::MAX
    } else {
        args.watch_full_scan_threshold
    };
    ReindexCoordinatorConfig::new(project_root.to_path_buf())
        .with_debounce(debounce)
        .with_max_debounce(max_debounce)
        .with_max_dirty_paths(maximum_dirty_paths)
}

#[derive(Debug)]
enum WatchSignal {
    Changed(Vec<PathBuf>),
    Full,
    Overflow,
}

async fn watch_and_reindex(
    coordinator: ReindexCoordinator,
    scan_roots: Vec<PathBuf>,
    project_root: PathBuf,
    index_root: PathBuf,
) -> anyhow::Result<()> {
    let (sender, mut receiver) = mpsc::channel::<WatchSignal>(WATCH_CHANNEL_CAPACITY);
    let overflowed = Arc::new(AtomicBool::new(false));
    let callback_overflowed = Arc::clone(&overflowed);
    let event_sender = sender.clone();
    let event_project_root = project_root.clone();

    let mut watcher =
        notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            let signal = match event {
                Ok(event) => watch_signal(event, &event_project_root, &index_root),
                Err(_) => Some(WatchSignal::Overflow),
            };
            let Some(signal) = signal else {
                return;
            };
            match event_sender.try_send(signal) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    callback_overflowed.store(true, Ordering::Release);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        })?;

    for root in &scan_roots {
        watcher.watch(root, notify::RecursiveMode::Recursive)?;
    }

    // Default Unity scan roots do not include the project root, so changes to root ignore files
    // need a separate non-recursive watch. Other root entries are intentionally discarded.
    let _root_watcher = if scan_roots.iter().any(|root| root == &project_root) {
        None
    } else {
        let root_sender = sender.clone();
        let root_overflowed = Arc::clone(&overflowed);
        let watched_project_root = project_root.clone();
        let mut root_watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                let signal = match event {
                    Ok(event)
                        if event.paths.iter().any(|path| {
                            is_project_root_ignore_path(&watched_project_root, path)
                        }) =>
                    {
                        Some(WatchSignal::Full)
                    }
                    Ok(_) => None,
                    Err(_) => Some(WatchSignal::Overflow),
                };
                let Some(signal) = signal else {
                    return;
                };
                match root_sender.try_send(signal) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        root_overflowed.store(true, Ordering::Release);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            })?;
        root_watcher.watch(&project_root, notify::RecursiveMode::NonRecursive)?;
        Some(root_watcher)
    };
    drop(sender);

    while let Some(signal) = receiver.recv().await {
        if overflowed.swap(false, Ordering::AcqRel) {
            while receiver.try_recv().is_ok() {}
            admit_watcher_overflow(&coordinator).await;
            continue;
        }
        match signal {
            WatchSignal::Changed(paths) => {
                if let Err(error) = coordinator
                    .admit(
                        ReindexSource::Watcher,
                        FilesystemReindexIntent::changed_paths(paths),
                    )
                    .await
                {
                    eprintln!("watcher reindex admission failed: {error}");
                }
            }
            WatchSignal::Full => {
                if let Err(error) = coordinator
                    .admit(ReindexSource::Watcher, FilesystemReindexIntent::full())
                    .await
                {
                    eprintln!("watcher full reindex admission failed: {error}");
                }
            }
            WatchSignal::Overflow => admit_watcher_overflow(&coordinator).await,
        }
    }
    Ok(())
}

fn watch_signal(
    event: notify::Event,
    project_root: &Path,
    index_root: &Path,
) -> Option<WatchSignal> {
    let paths = event
        .paths
        .into_iter()
        .filter(|path| !path.starts_with(index_root))
        .collect::<Vec<_>>();
    let force_full = paths
        .iter()
        .any(|path| is_project_root_ignore_path(project_root, path));
    if force_full {
        return Some(WatchSignal::Full);
    }

    (!paths.is_empty()).then_some(WatchSignal::Changed(paths))
}

fn is_project_root_ignore_path(project_root: &Path, path: &Path) -> bool {
    path.parent() == Some(project_root)
        && path.file_name().is_some_and(|name| {
            name == ".gitignore" || name == ".ignore" || name == ".unity-asset-search-ignore"
        })
}

async fn admit_watcher_overflow(coordinator: &ReindexCoordinator) {
    if let Err(error) = coordinator.watcher_overflow().await {
        eprintln!("watcher overflow admission failed: {error}");
    }
}

async fn reconcile_loop(coordinator: ReindexCoordinator, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        if let Err(error) = coordinator
            .admit(ReindexSource::Timer, FilesystemReindexIntent::reconcile())
            .await
        {
            eprintln!("timer reindex admission failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::is_project_root_ignore_path;

    #[test]
    fn only_project_root_ignore_files_trigger_policy_reconciliation() {
        let root = Path::new("project");

        assert!(is_project_root_ignore_path(
            root,
            Path::new("project/.gitignore")
        ));
        assert!(is_project_root_ignore_path(
            root,
            Path::new("project/.ignore")
        ));
        assert!(is_project_root_ignore_path(
            root,
            Path::new("project/.unity-asset-search-ignore")
        ));
        assert!(!is_project_root_ignore_path(
            root,
            Path::new("project/Assets/.gitignore")
        ));
        assert!(!is_project_root_ignore_path(
            root,
            Path::new("project/README.md")
        ));
    }
}
