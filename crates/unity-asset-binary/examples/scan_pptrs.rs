//! Scan `PPtr` references in a single object without fully parsing it.
//!
//! Run:
//! `cargo run -p unity-asset-binary --example scan_pptrs -- <path> <path_id> [asset_index]`
//!
//! - If `<path>` is an AssetBundle, `asset_index` defaults to 0.
//! - If `<path>` is a SerializedFile, `asset_index` is ignored.

mod common;

use std::path::PathBuf;
use unity_asset_binary::Result;
use unity_asset_binary::file::{UnityFile, load_unity_file_with_budget};
use unity_asset_binary::object::ObjectHandle;
use unity_asset_core::AssetLoadBudget;

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| unity_asset_binary::BinaryError::invalid_format("missing <path>"))?;
    let path_id: i64 = std::env::args()
        .nth(2)
        .ok_or_else(|| unity_asset_binary::BinaryError::invalid_format("missing <path_id>"))?
        .parse()
        .map_err(|e| {
            unity_asset_binary::BinaryError::invalid_format(format!("bad path_id: {e}"))
        })?;
    let asset_index: usize = std::env::args()
        .nth(3)
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|e| {
            unity_asset_binary::BinaryError::invalid_format(format!("bad asset_index: {e}"))
        })?
        .unwrap_or(0);

    let mut budget = AssetLoadBudget::default();
    let file = load_unity_file_with_budget(&path, &mut budget)?;
    println!("path: {}", path.display());
    println!("kind: {:?}", file.kind());
    println!("path_id: {}", path_id);

    let dump = |handle: ObjectHandle<'_>, budget: &mut AssetLoadBudget| -> Result<()> {
        println!("class_id: {}", handle.class_id());
        // Name discovery is optional, so malformed or unsupported object schemas are reported and
        // skipped. Resource, I/O, and other operation-wide failures remain terminal.
        let name = common::peek_name_best_effort(&handle, budget)?;
        println!("peek_name: {}", name.as_deref().unwrap_or("<unavailable>"));

        let Some(pptrs) = handle.scan_pptrs(budget)? else {
            println!("pptrs: <none>");
            return Ok(());
        };

        println!(
            "pptrs: internal={} external={}",
            pptrs.internal.len(),
            pptrs.external.len()
        );

        for path_id in pptrs.internal.iter().take(50) {
            println!("  - file_id=0 path_id={}", path_id);
        }
        for (file_id, path_id) in pptrs.external.iter().take(50) {
            println!("  - file_id={} path_id={}", file_id, path_id);
        }

        Ok(())
    };

    match &file {
        UnityFile::SerializedFile(sf) => {
            let handle = sf
                .find_object_handle(path_id)
                .ok_or_else(|| unity_asset_binary::BinaryError::invalid_data("object not found"))?;
            dump(handle, &mut budget)?;
        }
        UnityFile::AssetBundle(bundle) => {
            let sf = bundle.assets.get(asset_index).ok_or_else(|| {
                unity_asset_binary::BinaryError::invalid_data(format!(
                    "bundle has no asset_index={}",
                    asset_index
                ))
            })?;
            let handle = sf
                .find_object_handle(path_id)
                .ok_or_else(|| unity_asset_binary::BinaryError::invalid_data("object not found"))?;
            dump(handle, &mut budget)?;
        }
        UnityFile::WebFile(_) => {
            return Err(unity_asset_binary::BinaryError::invalid_format(
                "WebFile container: inspect entries through AssetWorkspace or the typed CLI",
            ));
        }
    };

    Ok(())
}
