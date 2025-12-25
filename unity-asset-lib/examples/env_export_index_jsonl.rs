//! Export a stable JSONL index of loaded binary objects (fast, name via `peek_name`).
//!
//! Run:
//! `cargo run -p unity-asset --example env_export_index_jsonl -- <path> [limit]`
//!
//! Output: one JSON object per line.

use serde::Serialize;
use std::path::PathBuf;
use unity_asset::environment::Environment;
use unity_asset::get_class_name;

#[derive(Debug, Serialize)]
struct BinaryIndexEntry {
    source_kind: String,
    source: String,
    asset_index: Option<usize>,
    path_id: i64,
    class_id: i32,
    class_name: String,
    name: Option<String>,
}

fn main() -> unity_asset::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/samples"));
    let limit: usize = std::env::args()
        .nth(2)
        .as_deref()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    let mut env = Environment::new();
    env.load(&path)?;

    let mut entries: Vec<BinaryIndexEntry> = env
        .binary_object_infos()
        .map(|obj_ref| {
            let key = obj_ref.key();
            let name = env.peek_binary_object_name(&key).ok().flatten();
            let class_id = obj_ref.object.class_id();
            BinaryIndexEntry {
                source_kind: format!("{:?}", key.source_kind),
                source: key.source.describe(),
                asset_index: key.asset_index,
                path_id: key.path_id,
                class_id,
                class_name: get_class_name(class_id).unwrap_or_else(|| format!("Class_{class_id}")),
                name,
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        (
            &a.source_kind,
            &a.source,
            a.asset_index,
            a.path_id,
            a.class_id,
        )
            .cmp(&(
                &b.source_kind,
                &b.source,
                b.asset_index,
                b.path_id,
                b.class_id,
            ))
    });

    let out = if limit == 0 {
        entries.as_slice()
    } else {
        &entries[..entries.len().min(limit)]
    };

    for entry in out {
        println!("{}", serde_json::to_string(entry).unwrap());
    }

    Ok(())
}
