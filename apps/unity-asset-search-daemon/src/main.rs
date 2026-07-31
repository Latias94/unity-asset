use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;

use unity_asset_search_daemon::coordinator::ReindexCoordinatorConfig;
use unity_asset_search_daemon::lifecycle::{DaemonRuntime, DaemonRuntimeConfig};
use unity_asset_search_daemon::watcher::WatcherConfig;
use unity_asset_search_index::{
    AssetLoadBudget, FilesystemReindexIntent, IndexPaths, SearchIndex, SearchIndexOptions,
};
use unity_asset_search_local::{PrivateRootsV1, ProjectLocatorV1, generate_daemon_instance_id};

const DEFAULT_RECONCILE_INTERVAL_MS: u64 = 5 * 60 * 1_000;

#[derive(Debug, Parser)]
#[command(name = "unity-asset-search-daemon")]
struct Args {
    #[arg(long)]
    project_root: PathBuf,

    /// Private base directory under which a project-bound index directory is derived.
    #[arg(long, value_name = "PRIVATE_BASE")]
    index_dir: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    scan_root: Vec<PathBuf>,

    #[arg(long)]
    scan_all: bool,

    /// Skip the initial reconciliation performed before serving requests.
    #[arg(long)]
    no_startup_reindex: bool,

    #[arg(long)]
    watch: bool,

    #[arg(long, default_value_t = 1500)]
    watch_debounce_ms: u64,

    /// Maximum dirty paths retained by the coordinator before escalating to a full reindex.
    ///
    /// Set to 0 to disable this threshold.
    #[arg(long, default_value_t = 5000)]
    watch_full_scan_threshold: usize,

    /// Periodically reconcile the project independently of filesystem watching.
    ///
    /// This repairs missed watcher events and transient build failures. Set to 0 to disable.
    #[arg(long, default_value_t = DEFAULT_RECONCILE_INTERVAL_MS)]
    reconcile_interval_ms: u64,

    /// Also index AssetBundle `m_Container` asset paths.
    #[arg(long)]
    index_bundle_container_entries: bool,

    /// Cap indexed container entries per bundle.
    #[arg(long, default_value_t = 50_000)]
    max_bundle_container_entries_per_bundle: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let project = ProjectLocatorV1::open(&args.project_root)?;
    let roots = PrivateRootsV1::discover_for_current_context()?;
    let namespace = roots.runtime().endpoint_namespace(project.project_id())?;
    let endpoint_claim = namespace.claim_daemon_endpoint()?;
    let stale_cleanup = endpoint_claim.stale_cleanup();
    if stale_cleanup == unity_asset_search_local::EndpointCleanupV1::Removed {
        eprintln!("retired stale endpoint descriptor before daemon initialization");
    }
    let daemon_instance_id = generate_daemon_instance_id()?;

    let scan_roots = if args.scan_all {
        Some(vec![PathBuf::from(".")])
    } else if args.scan_root.is_empty() {
        None
    } else {
        Some(args.scan_root.clone())
    };
    let paths = IndexPaths::for_project(
        project.root().to_path_buf(),
        args.index_dir.clone(),
        scan_roots,
    )?;
    let options = SearchIndexOptions {
        index_bundle_container_entries: args.index_bundle_container_entries,
        max_bundle_container_entries_per_bundle: args.max_bundle_container_entries_per_bundle,
        ..SearchIndexOptions::default()
    };

    let mut open_budget = AssetLoadBudget::default();
    let index = SearchIndex::open_or_create_with_options(paths.clone(), options, &mut open_budget)?;
    project.revalidate()?;
    let coordinator_config = coordinator_config(&args, paths.project_root());
    let watcher = args.watch.then(|| WatcherConfig {
        scan_roots: paths.scan_roots().to_vec(),
        project_root: paths.project_root().to_path_buf(),
        index_namespace_exclusion: paths.index_namespace_exclusion().map(PathBuf::from),
    });
    let reconcile_interval = reconciliation_interval(&args);
    let startup_reindex = (!args.no_startup_reindex).then(FilesystemReindexIntent::reconcile);
    let runtime_config = DaemonRuntimeConfig::new(
        endpoint_claim,
        daemon_instance_id,
        index,
        coordinator_config,
    )
    .with_startup_reindex(startup_reindex)
    .with_watcher(watcher)
    .with_reconcile_interval(reconcile_interval);
    let mut runtime = DaemonRuntime::start(runtime_config)?;
    let report = runtime.run().await?;
    eprintln!("endpoint cleanup: {:?}", report.endpoint_cleanup);
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

fn reconciliation_interval(args: &Args) -> Option<Duration> {
    (args.reconcile_interval_ms > 0).then(|| Duration::from_millis(args.reconcile_interval_ms))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::Parser as _;

    use super::{Args, DEFAULT_RECONCILE_INTERVAL_MS, reconciliation_interval};

    #[test]
    fn periodic_reconciliation_is_independent_of_watching_and_can_be_disabled() {
        let defaults =
            Args::try_parse_from(["unity-asset-search-daemon", "--project-root", "."]).unwrap();
        assert!(!defaults.watch);
        assert_eq!(
            reconciliation_interval(&defaults),
            Some(Duration::from_millis(DEFAULT_RECONCILE_INTERVAL_MS))
        );

        let disabled = Args::try_parse_from([
            "unity-asset-search-daemon",
            "--project-root",
            ".",
            "--reconcile-interval-ms",
            "0",
        ])
        .unwrap();
        assert_eq!(reconciliation_interval(&disabled), None);
    }
}
