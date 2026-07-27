use std::path::PathBuf;
use std::time::Instant;

use unity_asset_core::AssetLoadBudget;
use unity_asset_search_index::{
    FilesystemReindexIntent, IndexPaths, SearchIndex, SearchIndexOptions,
};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let project_root: PathBuf = match args.next() {
        Some(p) => p.into(),
        None => {
            eprintln!(
                "usage: reindex_project <PROJECT_ROOT> [INDEX_DIR]\n\nExample:\n  cargo run -p unity-asset-search-index --example reindex_project -- C:\\\\path\\\\to\\\\UnityProject C:\\\\path\\\\to\\\\index-dir"
            );
            std::process::exit(2);
        }
    };
    let index_dir: Option<PathBuf> = args.next().map(PathBuf::from);

    let paths = IndexPaths::for_project(project_root, index_dir, None)?;
    eprintln!("project_root: {}", paths.project_root().display());
    eprintln!("index_dir: {}", paths.index_root().display());
    eprintln!(
        "scan_roots: {}",
        paths
            .scan_roots()
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let index_bundle_container_entries = std::env::var("UNITY_ASSET_INDEX_BUNDLE_CONTAINER")
        .ok()
        .is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"));
    let max_bundle_container_entries_per_bundle: usize =
        std::env::var("UNITY_ASSET_MAX_BUNDLE_CONTAINER_ENTRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50_000);
    let respect_project_root_ignore_files =
        std::env::var("UNITY_ASSET_RESPECT_PROJECT_ROOT_IGNORE_FILES")
            .ok()
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
    let respect_project_root_gitignore =
        std::env::var("UNITY_ASSET_RESPECT_PROJECT_ROOT_GITIGNORE")
            .ok()
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);

    let mut budget = AssetLoadBudget::default();
    let index = SearchIndex::open_or_create_with_options(
        paths,
        SearchIndexOptions {
            index_bundle_container_entries,
            max_bundle_container_entries_per_bundle,
            respect_project_root_ignore_files,
            respect_project_root_gitignore,
            ..SearchIndexOptions::default()
        },
        &mut budget,
    )?;

    let start = Instant::now();
    let receipt = index.reindex(FilesystemReindexIntent::full(), &mut budget)?;
    let elapsed = start.elapsed();

    let status = index.status()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "receipt": receipt,
            "status": status,
            "elapsed_ms_wall": elapsed.as_millis(),
        }))?
    );

    Ok(())
}
