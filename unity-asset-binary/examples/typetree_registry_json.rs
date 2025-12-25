//! Demonstrate JSON TypeTree registry for stripped assets (best-effort).
//!
//! This example:
//! 1) loads a file,
//! 2) captures a TypeTree for a `Texture2D`,
//! 3) simulates a stripped file by clearing TypeTrees,
//! 4) re-attaches the TypeTree via a JSON registry.
//!
//! Run:
//! `cargo run -p unity-asset-binary --example typetree_registry_json -- <path>`
//!
//! If no path is provided, it defaults to `tests/samples/banner_1`.

use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use unity_asset_binary::file::{UnityFile, load_unity_file};
use unity_asset_binary::typetree::JsonTypeTreeRegistry;

#[derive(Debug, Serialize)]
struct Dump {
    schema: u32,
    entries: Vec<Entry>,
}

#[derive(Debug, Serialize)]
struct Entry {
    #[serde(skip_serializing_if = "Option::is_none")]
    unity_version: Option<String>,
    class_id: i32,
    type_tree: unity_asset_binary::typetree::TypeTree,
}

fn main() -> unity_asset_binary::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/samples/banner_1"));

    let mut file = load_unity_file(&path)?;
    let sf = match &mut file {
        UnityFile::SerializedFile(sf) => sf,
        UnityFile::AssetBundle(bundle) => bundle.assets.get_mut(0).ok_or_else(|| {
            unity_asset_binary::BinaryError::invalid_data("bundle has no asset 0")
        })?,
        UnityFile::WebFile(_) => {
            return Err(unity_asset_binary::BinaryError::invalid_format(
                "WebFile container: pick an entry and parse it via unity-asset Environment/CLI",
            ));
        }
    };

    let texture = sf
        .object_handles()
        .find(|h| h.class_id() == 28)
        .ok_or_else(|| unity_asset_binary::BinaryError::invalid_data("no Texture2D found"))?;
    let path_id = texture.path_id();

    let type_tree = sf
        .types
        .iter()
        .find(|t| t.class_id == 28)
        .ok_or_else(|| unity_asset_binary::BinaryError::invalid_data("no Texture2D type"))?
        .type_tree
        .clone();

    println!("path: {}", path.display());
    println!("unity_version: {}", sf.unity_version);
    println!("texture_path_id: {}", path_id);
    println!(
        "peek_name (typetree): {}",
        sf.find_object_handle(path_id)
            .ok_or_else(|| unity_asset_binary::BinaryError::invalid_data("object not found"))?
            .peek_name()?
            .unwrap_or_default()
    );

    sf.enable_type_tree = false;
    for t in sf.types.iter_mut() {
        t.type_tree.clear();
    }
    sf.set_type_tree_registry(None);

    let stripped = sf
        .find_object_handle(path_id)
        .ok_or_else(|| unity_asset_binary::BinaryError::invalid_data("object not found"))?
        .peek_name()?;
    println!("peek_name (stripped): {:?}", stripped);

    let tmp = tempfile::tempdir().map_err(|e| {
        unity_asset_binary::BinaryError::generic(format!("Failed to create tempdir: {}", e))
    })?;
    let reg_path = tmp.path().join("typetree_registry.json");
    let dump = Dump {
        schema: 1,
        entries: vec![Entry {
            unity_version: None,
            class_id: 28,
            type_tree,
        }],
    };
    std::fs::write(&reg_path, serde_json::to_string_pretty(&dump).unwrap()).map_err(|e| {
        unity_asset_binary::BinaryError::generic(format!("Failed to write registry JSON: {}", e))
    })?;

    let registry = JsonTypeTreeRegistry::from_path(&reg_path)?;
    sf.set_type_tree_registry(Some(Arc::new(registry)));

    let restored = sf
        .find_object_handle(path_id)
        .ok_or_else(|| unity_asset_binary::BinaryError::invalid_data("object not found"))?
        .peek_name()?;
    println!("peek_name (registry): {:?}", restored);

    let obj = sf
        .find_object_handle(path_id)
        .ok_or_else(|| unity_asset_binary::BinaryError::invalid_data("object not found"))?
        .read()?;
    let w = obj.get("m_Width").and_then(|v| v.as_i64());
    let h = obj.get("m_Height").and_then(|v| v.as_i64());
    println!("parsed dimensions: width={:?} height={:?}", w, h);

    Ok(())
}
