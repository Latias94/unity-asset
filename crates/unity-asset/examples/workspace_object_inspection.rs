//! Emit versioned, revision-bound object inspections as deterministic JSONL.
//!
//! Run:
//! `cargo run -p unity-asset --example workspace_object_inspection -- <path> [path_id] [limit]`
//!
//! A binary `path_id` filter may still match multiple SerializedFiles. Every exact match is
//! retained and ordered by `ObjectAddress`; no source-global uniqueness is assumed. A limit of
//! zero means unlimited.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use unity_asset::AssetLoadBudget;
use unity_asset::workspace::{AssetWorkspace, WorkspaceInspector, WorkspaceObjectFormatInspection};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (path, path_id, limit) = arguments()?;
    let mut budget = AssetLoadBudget::default();
    let mut workspace = AssetWorkspace::new()?;
    workspace.load_path(&path, &mut budget)?;
    let snapshot = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&snapshot);
    let mut objects = inspector.objects(&mut budget)?;
    if let Some(expected) = path_id {
        objects.retain(|object| {
            matches!(
                object.format(),
                WorkspaceObjectFormatInspection::Binary { path_id, .. }
                    if path_id == expected
            )
        });
    }
    objects.sort_by(|left, right| left.address().cmp(right.address()));

    let written = objects.len().min(limit);
    eprintln!(
        "workspace={} revision={} matches={} written={written}",
        snapshot.workspace_id(),
        snapshot.revision(),
        objects.len()
    );

    let stdout = io::stdout();
    let mut output = stdout.lock();
    for object in objects.iter().take(limit) {
        serde_json::to_writer(&mut output, object)?;
        output.write_all(b"\n")?;
    }
    output.flush()?;

    Ok(())
}

fn arguments() -> io::Result<(PathBuf, Option<i64>, usize)> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments.next().map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: workspace_object_inspection <path> [path_id] [limit]",
        )
    })?;
    let path_id = arguments
        .next()
        .map(|value| parse_i64(value, "path_id"))
        .transpose()?;
    let limit = arguments
        .next()
        .map(parse_limit)
        .transpose()?
        .filter(|limit| *limit != 0)
        .unwrap_or(usize::MAX);
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: workspace_object_inspection <path> [path_id] [limit]",
        ));
    }
    Ok((path, path_id, limit))
}

fn parse_i64(value: OsString, label: &str) -> io::Result<i64> {
    let value = value.into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} must be UTF-8"),
        )
    })?;
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {label} {value:?}: {error}"),
        )
    })
}

fn parse_limit(value: OsString) -> io::Result<usize> {
    let value = value
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "limit must be UTF-8"))?;
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid limit {value:?}: {error}"),
        )
    })
}
