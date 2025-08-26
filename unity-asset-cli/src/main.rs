//! Unity Asset Parser CLI
//!
//! Command-line interface for parsing and manipulating Unity assets.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use unity_asset::{UnityDocument, YamlDocument};

#[derive(Parser)]
#[command(name = "unity_asset")]
#[command(about = "A Rust-based Unity asset parser")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Parse a Unity YAML file
    ParseYaml {
        /// Input YAML file path
        #[arg(short, long)]
        input: PathBuf,

        /// Output format (json, yaml, debug)
        #[arg(short, long, default_value = "debug")]
        format: String,

        /// Preserve original types instead of converting to strings
        #[arg(long)]
        preserve_types: bool,
    },

    /// Extract information from Unity files
    Extract {
        /// Input file or directory path
        #[arg(short, long)]
        input: PathBuf,

        /// Output directory
        #[arg(short, long)]
        output: PathBuf,

        /// Unity class types to extract (GameObject, Transform, etc.)
        #[arg(long)]
        types: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::ParseYaml {
            input,
            format,
            preserve_types,
        } => parse_yaml_command(input, format, preserve_types),
        Commands::Extract {
            input,
            output,
            types,
        } => extract_command(input, output, types),
    }
}

fn parse_yaml_command(input: PathBuf, format: String, preserve_types: bool) -> Result<()> {
    println!("Parsing YAML file: {:?}", input);
    println!("Output format: {}", format);
    println!("Preserve types: {}", preserve_types);

    // Load the YAML document
    let doc = unity_asset::YamlDocument::load_yaml(&input, preserve_types)?;

    println!("✓ Successfully loaded YAML document");
    println!("  Entries: {}", doc.entries().len());

    // Display entries based on format
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
            // Convert to JSON format for easier processing
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

fn extract_command(input: PathBuf, output: PathBuf, types: Vec<String>) -> Result<()> {
    println!("Extracting from: {:?}", input);
    println!("Output to: {:?}", output);
    println!("Types: {:?}", types);

    // Create output directory if it doesn't exist
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            unity_asset::UnityAssetError::format(format!(
                "Failed to create output directory: {}",
                e
            ))
        })?;
    }

    // Try to load as different file types
    let extension = input.extension().and_then(|s| s.to_str()).unwrap_or("");

    match extension {
        "asset" | "prefab" | "unity" | "meta" => {
            // Load as YAML document
            let doc = unity_asset::YamlDocument::load_yaml(&input, false)?;
            println!(
                "✓ Loaded YAML document with {} entries",
                doc.entries().len()
            );

            // Filter by types if specified
            let entries_to_extract: Vec<_> = if types.is_empty() {
                doc.entries().iter().collect()
            } else {
                doc.filter(
                    Some(&types.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
                    None,
                )
            };

            println!("✓ Found {} entries to extract", entries_to_extract.len());

            // Extract each entry
            for (i, entry) in entries_to_extract.iter().enumerate() {
                let filename = format!("{}_{:03}_{}.yaml", entry.class_name, i, entry.anchor);
                let entry_path = output.join(filename);

                // Create a single-entry document
                let mut single_doc = unity_asset::YamlDocument::new();
                single_doc.add_entry((*entry).clone());

                // Save the entry
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
