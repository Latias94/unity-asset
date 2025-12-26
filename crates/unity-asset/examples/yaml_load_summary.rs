//! YAML load summary example.
//!
//! Run:
//! `cargo run -p unity-asset --example yaml_load_summary -- <path-to-yaml>`
//!
//! If no path is provided, a small repo fixture is used.

use std::path::PathBuf;
use unity_asset::{UnityDocument, YamlDocument};

fn main() -> unity_asset::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab")
        });

    let (doc, warnings) = YamlDocument::load_yaml_with_warnings(&path, false)?;
    println!("loaded: {}", path.display());
    println!("documents: 1");
    println!("objects: {}", doc.entries().len());

    if warnings.is_empty() {
        println!("warnings: 0");
    } else {
        println!("warnings: {}", warnings.len());
        for w in warnings {
            println!("  - doc_index={}: {}", w.doc_index, w.error);
        }
    }

    for obj in doc.entries().iter().take(5) {
        let name = obj.get("m_Name").and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "object: class={} anchor=&{} name={}",
            obj.class_name, obj.anchor, name
        );
    }

    Ok(())
}
