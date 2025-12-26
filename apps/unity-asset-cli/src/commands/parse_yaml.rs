use crate::shared::{AppContext, cli_warn};
use anyhow::Result;
use std::path::PathBuf;
use unity_asset::UnityDocument;

pub(crate) fn run(
    input: PathBuf,
    format: String,
    preserve_types: bool,
    ctx: &AppContext,
) -> Result<()> {
    println!("Parsing YAML file: {:?}", input);
    println!("Output format: {}", format);
    println!("Preserve types: {}", preserve_types);

    let (doc, warnings) =
        unity_asset::YamlDocument::load_yaml_with_warnings(&input, preserve_types)?;
    if ctx.show_warnings {
        for w in warnings {
            cli_warn(ctx.show_warnings, w);
        }
    }

    println!("✓ Successfully loaded YAML document");
    println!("  Entries: {}", doc.entries().len());

    match format.as_str() {
        "summary" => {
            for (i, entry) in doc.entries().iter().enumerate() {
                println!(
                    "  [{}]: {} (ID: {}, Anchor: {})",
                    i, entry.class_name, entry.class_id, entry.anchor
                );
            }
        }
        "detailed" => {
            for (i, entry) in doc.entries().iter().enumerate() {
                println!(
                    "  [{}]: {} (ID: {}, Anchor: {})",
                    i, entry.class_name, entry.class_id, entry.anchor
                );
                let props = entry.properties();
                println!("    Properties: {}", props.len());
                for (key, value) in props.iter().take(5) {
                    println!("      {}: {:?}", key, value);
                }
                if props.len() > 5 {
                    println!("      ... and {} more properties", props.len() - 5);
                }
            }
        }
        "json" => {
            println!("JSON output not yet implemented");
        }
        _ => {
            println!(
                "Unknown format: {}. Supported formats: summary, detailed, json",
                format
            );
        }
    }

    Ok(())
}
