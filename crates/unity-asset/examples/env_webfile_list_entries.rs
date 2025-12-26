//! List WebFile entries and loaded embedded sources.
//!
//! Run:
//! `cargo run -p unity-asset --example env_webfile_list_entries -- <path-to-UnityWebData>`

use std::path::PathBuf;
use unity_asset::environment::Environment;

fn main() -> unity_asset::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| unity_asset::UnityAssetError::format("missing <path>"))?;

    let mut env = Environment::new();
    env.load(&path)?;

    println!("loaded: {}", path.display());
    println!("webfiles: {}", env.webfiles().len());
    println!("bundles: {}", env.bundles().len());
    println!("binary_assets: {}", env.binary_assets().len());
    println!("warnings: {}", env.warnings().len());
    println!();

    for (web_path, web) in env.webfiles().iter() {
        println!("webfile: {}", web_path.display());
        println!("entries: {}", web.files.len());
        for f in web.files.iter().take(20) {
            println!("  - {} (size={})", f.name, f.size);
        }
        println!();
    }

    let sources = env.binary_sources();
    println!("loaded_binary_sources: {}", sources.len());
    for (kind, source) in sources.into_iter().take(20) {
        println!("  - kind={:?} source={}", kind, source.describe());
    }

    Ok(())
}
