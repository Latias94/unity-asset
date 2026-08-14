//! YAML load summary example.
//!
//! Run:
//! `cargo run -p unity-asset --example yaml_load_summary -- <path-to-yaml>`
//!
//! If no path is provided, a small repo fixture is used.

use std::path::PathBuf;
use unity_asset::AssetLoadBudget;
use unity_asset_yaml::load_budgeted_yaml_path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab")
        });

    let mut budget = AssetLoadBudget::default();
    let source = load_budgeted_yaml_path(&path, &mut budget)?;
    let doc = source.document();
    println!("loaded: {}", path.display());
    println!("documents: 1");
    println!("objects: {}", doc.entries().len());
    println!("budgeted bytes: {}", budget.usage().bytes);

    for obj in doc.entries().iter().take(5) {
        let name = obj.get("m_Name").and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "object: class={} anchor=&{} name={}",
            obj.class_name(),
            obj.anchor(),
            name
        );
    }

    Ok(())
}
