//! Inspect one Unity source and every member it contributes to the workspace catalog.
//!
//! Run:
//! `cargo run -p unity-asset --example workspace_source_inventory -- <path>`
//!
//! Container members and streamed-resource sidecars appear as independent source records with
//! stable locators and parent identities. JSONL is written to stdout; the revision summary is
//! written to stderr.

use std::io::{self, Write};
use std::path::PathBuf;

use unity_asset::AssetLoadBudget;
use unity_asset::workspace::{AssetWorkspace, WorkspaceInspector};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: workspace_source_inventory <path>",
            )
        })?;

    let mut budget = AssetLoadBudget::default();
    let mut workspace = AssetWorkspace::new()?;
    let root = workspace.load_path(&path, &mut budget)?;
    let snapshot = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&snapshot);
    let mut sources = inspector.sources(&mut budget)?;
    sources.sort_by(|left, right| left.source().locator().cmp(right.source().locator()));

    eprintln!(
        "workspace={} revision={} root={root:?} sources={}",
        snapshot.workspace_id(),
        snapshot.revision(),
        sources.len()
    );

    let stdout = io::stdout();
    let mut output = stdout.lock();
    for source in &sources {
        serde_json::to_writer(&mut output, source)?;
        output.write_all(b"\n")?;
    }
    output.flush()?;

    Ok(())
}
