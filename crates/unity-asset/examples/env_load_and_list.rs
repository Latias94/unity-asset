//! Environment load + list example.
//!
//! Run:
//! `cargo run -p unity-asset --example env_load_and_list -- <path>`
//!
//! `<path>` can be a file or directory. If omitted, `tests/samples` is used.

use std::path::PathBuf;
use unity_asset::environment::{Environment, EnvironmentObjectRef};

fn main() -> unity_asset::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/samples"));

    let mut env = Environment::new();
    env.load(&path)?;

    println!("loaded: {}", path.display());
    println!("yaml_documents: {}", env.yaml_documents().len());
    println!("binary_assets: {}", env.binary_assets().len());
    println!("bundles: {}", env.bundles().len());
    println!("webfiles: {}", env.webfiles().len());

    let warnings = env.warnings();
    println!("warnings: {}", warnings.len());

    println!();
    println!("first objects:");
    for obj in env.objects().take(20) {
        match obj {
            EnvironmentObjectRef::Yaml(v) => {
                let name = v.get("m_Name").and_then(|x| x.as_str()).unwrap_or("");
                println!(
                    "yaml: class={} anchor=&{} name={}",
                    v.class_name, v.anchor, name
                );
            }
            EnvironmentObjectRef::Binary(v) => {
                let key = v.key();
                let name = env.peek_binary_object_name(&key).ok().flatten();
                println!(
                    "bin: kind={:?} source={} path_id={} class_id={} name={}",
                    key.source_kind,
                    key.source.describe(),
                    key.path_id,
                    v.object.class_id(),
                    name.unwrap_or_default()
                );
            }
        }
    }

    Ok(())
}
