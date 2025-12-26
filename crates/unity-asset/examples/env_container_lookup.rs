//! Bundle container lookup example (UnityPy-like discovery).
//!
//! Run:
//! `cargo run -p unity-asset --example env_container_lookup -- <path> <pattern>`
//!
//! `<pattern>` is a substring match against `AssetBundle.m_Container` asset paths.
//! If omitted, it defaults to `Assets/`.

use std::path::PathBuf;
use unity_asset::environment::Environment;

fn main() -> unity_asset::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/samples"));
    let pattern = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "Assets/".to_string());

    let mut env = Environment::new();
    env.load(&path)?;

    let mut entries = env.find_binary_object_keys_in_bundle_container(&pattern);
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    println!("loaded: {}", path.display());
    println!("pattern: {}", pattern);
    println!("matches: {}", entries.len());

    for (asset_path, key) in entries.into_iter().take(20) {
        let name = env.peek_binary_object_name(&key).ok().flatten();
        println!(
            "{} -> source={} path_id={} name={}",
            asset_path,
            key.source.describe(),
            key.path_id,
            name.unwrap_or_default()
        );
    }

    Ok(())
}
