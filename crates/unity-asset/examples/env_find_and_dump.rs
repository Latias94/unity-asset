//! Find a binary object by `path_id` and dump its properties as JSON.
//!
//! Run:
//! `cargo run -p unity-asset --example env_find_and_dump -- <path> <path_id>`
//!
//! Notes:
//! - `path_id` is only unique within a single SerializedFile.
//! - If multiple sources contain the same `path_id`, this example prints all matching keys and
//!   dumps the first match.

use std::path::PathBuf;
use unity_asset::environment::Environment;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("missing <path>")?;
    let path_id: i64 = std::env::args()
        .nth(2)
        .ok_or("missing <path_id>")?
        .parse()?;

    let mut env = Environment::new();
    env.load(&path)?;

    let keys = env.find_binary_object_keys(path_id);
    if keys.is_empty() {
        println!("no matches for path_id={}", path_id);
        return Ok(());
    }

    println!("loaded: {}", path.display());
    println!("matches: {}", keys.len());
    for k in &keys {
        println!(
            "  - kind={:?} source={} asset_index={:?} path_id={}",
            k.source_kind,
            k.source.describe(),
            k.asset_index,
            k.path_id
        );
    }

    let obj = env.read_binary_object_key(&keys[0])?;
    println!();
    println!(
        "dump: class_id={} class_name={} path_id={}",
        obj.class_id(),
        obj.class_name(),
        obj.path_id()
    );

    let warnings = obj.typetree_warnings();
    println!("typetree_warnings: {}", warnings.len());
    for w in warnings.iter().take(10) {
        println!("  - {}: {}", w.field, w.error);
    }

    let json = serde_json::to_string_pretty(&obj.as_unity_class().serialized_properties())?;
    println!();
    println!("{}", json);

    Ok(())
}
