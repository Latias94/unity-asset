//! Load a Unity binary file and print a small summary.
//!
//! Run:
//! `cargo run -p unity-asset-binary --example load_and_list -- <path>`

use std::path::PathBuf;
use unity_asset_binary::file::{UnityFile, load_unity_file};

fn main() -> unity_asset_binary::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/samples/char_118_yuki.ab"));

    if path.is_dir() {
        return Err(unity_asset_binary::BinaryError::invalid_format(
            "Please provide a file path (not a directory)",
        ));
    }

    let file = load_unity_file(&path)?;
    println!("path: {}", path.display());
    println!("kind: {:?}", file.kind());

    match file {
        UnityFile::SerializedFile(sf) => {
            let stats = sf.statistics();
            println!("unity_version: {}", stats.unity_version);
            println!("has_type_tree: {}", stats.has_type_tree);
            println!("objects: {}", stats.object_count);
            println!("types: {}", stats.type_count);

            for h in sf.object_handles().take(10) {
                let name = h.peek_name().ok().flatten();
                println!(
                    "  - path_id={} class_id={} name={}",
                    h.path_id(),
                    h.class_id(),
                    name.unwrap_or_default()
                );
            }
        }
        UnityFile::AssetBundle(bundle) => {
            println!("assets: {}", bundle.assets.len());
            for (i, sf) in bundle.assets.iter().enumerate().take(3) {
                let stats = sf.statistics();
                println!(
                    "asset[{}]: unity_version={} has_type_tree={} objects={} types={}",
                    i,
                    stats.unity_version,
                    stats.has_type_tree,
                    stats.object_count,
                    stats.type_count
                );
            }
        }
        UnityFile::WebFile(web) => {
            println!("entries: {}", web.files.len());
            for f in web.files.iter().take(10) {
                println!("  - {} (size={})", f.name, f.size);
            }
        }
    }

    Ok(())
}
