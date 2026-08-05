//! Query exact `AssetBundle.m_Container` occurrences against a revision-bound reference graph.
//!
//! Run:
//! `cargo run -p unity-asset --example bundle_container_query -- <path> [pattern]`
//!
//! Patterns without `*` or `?` use a case-insensitive substring match. The canonical query
//! result preserves container order, raw PPtr values, resolution state, and diagnostics.

use std::io::{self, Write};
use std::path::PathBuf;

use unity_asset::AssetLoadBudget;
use unity_asset::extraction::{BundleContainerQuery, ExtractionPlanner};
use unity_asset::workspace::AssetWorkspace;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (path, pattern) = arguments()?;
    let mut budget = AssetLoadBudget::default();
    let mut workspace = AssetWorkspace::new()?;
    workspace.load_path(&path, &mut budget)?;
    let snapshot = workspace.snapshot();
    let planner = ExtractionPlanner::new(&snapshot);
    let result =
        planner.bundle_container_occurrences(BundleContainerQuery::new(pattern)?, &mut budget)?;

    eprintln!(
        "workspace={} revision={} complete={} occurrences={}",
        result.workspace_id(),
        result.revision(),
        result.is_complete(),
        result.occurrences().len()
    );

    let stdout = io::stdout();
    let mut output = stdout.lock();
    result.write_canonical_json(&mut output)?;
    output.write_all(b"\n")?;
    output.flush()?;

    Ok(())
}

fn arguments() -> io::Result<(PathBuf, String)> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments.next().map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: bundle_container_query <path> [pattern]",
        )
    })?;
    let pattern = arguments
        .next()
        .map(|value| {
            value
                .into_string()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pattern must be UTF-8"))
        })
        .transpose()?
        .unwrap_or_else(|| "Assets/".to_owned());
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: bundle_container_query <path> [pattern]",
        ));
    }
    Ok((path, pattern))
}
