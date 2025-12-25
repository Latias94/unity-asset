use crate::shared::AppContext;
use anyhow::Result;
use std::path::PathBuf;
use unity_asset::UnityDocument;

pub(crate) fn run(
    input: PathBuf,
    output: PathBuf,
    types: Vec<String>,
    _ctx: &AppContext,
) -> Result<()> {
    println!("Extracting from: {:?}", input);
    println!("Output to: {:?}", output);
    println!("Types: {:?}", types);

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            unity_asset::UnityAssetError::format(format!(
                "Failed to create output directory: {}",
                e
            ))
        })?;
    }

    let extension = input.extension().and_then(|s| s.to_str()).unwrap_or("");

    match extension {
        "asset" | "prefab" | "unity" | "meta" => {
            let doc = unity_asset::YamlDocument::load_yaml(&input, false)?;
            println!(
                "✓ Loaded YAML document with {} entries",
                doc.entries().len()
            );

            let entries_to_extract: Vec<_> = if types.is_empty() {
                doc.entries().iter().collect()
            } else {
                doc.filter(
                    Some(&types.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
                    None,
                )
            };

            println!("✓ Found {} entries to extract", entries_to_extract.len());

            for (i, entry) in entries_to_extract.iter().enumerate() {
                let filename = format!("{}_{:03}_{}.yaml", entry.class_name, i, entry.anchor);
                let entry_path = output.join(filename);

                let mut single_doc = unity_asset::YamlDocument::new();
                single_doc.add_entry((*entry).clone());

                single_doc.save_to(&entry_path)?;
                println!("  Extracted: {}", entry_path.display());
            }
        }
        _ => {
            println!("⚠ Unsupported file type: {}", extension);
            println!("  Supported types: .asset, .prefab, .unity, .meta");
        }
    }

    Ok(())
}
