//! Unity Asset Parser CLI - Async Version
//!
//! High-performance async command-line interface for parsing and manipulating Unity assets.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;
use unity_asset_core::UnityDocument;

#[cfg(feature = "async")]
use futures::stream::{self, StreamExt};
#[cfg(feature = "async")]
use indicatif::{ProgressBar, ProgressStyle};
#[cfg(feature = "async")]
use unity_asset_core::document::AsyncUnityDocument;

#[derive(Parser)]
#[command(name = "unity_asset_async")]
#[command(about = "A high-performance async Unity asset parser")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Number of concurrent operations (default: 8)
    #[arg(long, global = true)]
    concurrency: Option<usize>,

    /// Show progress bars
    #[arg(long, global = true)]
    progress: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Parse Unity YAML files (supports batch processing)
    ParseYaml {
        /// Input YAML file or directory path
        #[arg(short, long)]
        input: PathBuf,

        /// Output format (summary, detailed, json)
        #[arg(short, long, default_value = "summary")]
        format: String,

        /// Preserve original types instead of converting to strings
        #[arg(long)]
        preserve_types: bool,

        /// Process files recursively
        #[arg(short, long)]
        recursive: bool,
    },

    /// Extract information from Unity files with concurrent processing
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

        /// Process files recursively
        #[arg(short, long)]
        recursive: bool,
    },
}

#[cfg(feature = "async")]
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let concurrency = cli.concurrency.unwrap_or(8); // Default to 8 concurrent operations

    println!(
        "🚀 Unity Asset Parser (Async) - Concurrency: {}",
        concurrency
    );

    match cli.command {
        Commands::ParseYaml {
            input,
            format,
            preserve_types,
            recursive,
        } => {
            parse_yaml_command_async(
                input,
                format,
                preserve_types,
                recursive,
                concurrency,
                cli.progress,
            )
            .await
        }
        Commands::Extract {
            input,
            output,
            types,
            recursive,
        } => {
            extract_command_async(input, output, types, recursive, concurrency, cli.progress).await
        }
    }
}

#[cfg(not(feature = "async"))]
fn main() -> Result<()> {
    eprintln!("❌ Async features not enabled. Please compile with --features async");
    std::process::exit(1);
}

#[cfg(feature = "async")]
async fn parse_yaml_command_async(
    input: PathBuf,
    format: String,
    preserve_types: bool,
    recursive: bool,
    concurrency: usize,
    show_progress: bool,
) -> Result<()> {
    let start_time = Instant::now();

    println!("📂 Scanning for YAML files...");
    let yaml_files = collect_yaml_files(&input, recursive).await?;

    if yaml_files.is_empty() {
        println!("⚠️  No YAML files found in {:?}", input);
        return Ok(());
    }

    println!("📄 Found {} YAML files", yaml_files.len());

    let progress = if show_progress {
        let pb = ProgressBar::new(yaml_files.len() as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        Some(pb)
    } else {
        None
    };

    // Process files concurrently
    let file_count = yaml_files.len();
    let results = stream::iter(yaml_files)
        .map(|file_path| {
            let format = format.clone();
            let progress = progress.clone();
            async move {
                let result = process_single_yaml_file(&file_path, &format, preserve_types).await;
                if let Some(ref pb) = progress {
                    pb.inc(1);
                }
                (file_path, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

    if let Some(pb) = progress {
        pb.finish_with_message("✅ Processing complete");
    }

    // Report results
    let mut success_count = 0;
    let mut error_count = 0;

    for (file_path, result) in results {
        match result {
            Ok(entry_count) => {
                success_count += 1;
                if !show_progress {
                    println!("✅ {}: {} entries", file_path.display(), entry_count);
                }
            }
            Err(e) => {
                error_count += 1;
                eprintln!("❌ {}: {}", file_path.display(), e);
            }
        }
    }

    let elapsed = start_time.elapsed();
    println!("\n📊 Summary:");
    println!("  ✅ Success: {}", success_count);
    println!("  ❌ Errors: {}", error_count);
    println!("  ⏱️  Time: {:.2}s", elapsed.as_secs_f64());
    if elapsed.as_secs_f64() > 0.0 {
        println!(
            "  🚀 Throughput: {:.1} files/sec",
            file_count as f64 / elapsed.as_secs_f64()
        );
    }

    Ok(())
}

#[cfg(feature = "async")]
async fn process_single_yaml_file(
    file_path: &PathBuf,
    format: &str,
    preserve_types: bool,
) -> Result<usize> {
    let doc = unity_asset_yaml::YamlDocument::load_yaml_async(file_path, preserve_types).await?;
    let entry_count = UnityDocument::entries(&doc).len();

    match format {
        "summary" => {
            // Just return count for summary
        }
        "detailed" => {
            println!("\n📄 File: {}", file_path.display());
            for (i, entry) in UnityDocument::entries(&doc).iter().enumerate().take(3) {
                println!(
                    "  [{}]: {} (ID: {}, Anchor: {})",
                    i, entry.class_name, entry.class_id, entry.anchor
                );
            }
            if entry_count > 3 {
                println!("  ... and {} more entries", entry_count - 3);
            }
        }
        _ => {
            // Default behavior
        }
    }

    Ok(entry_count)
}

#[cfg(feature = "async")]
async fn collect_yaml_files(input: &PathBuf, recursive: bool) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if input.is_file() {
        if is_yaml_file(input) {
            files.push(input.clone());
        }
    } else if input.is_dir() {
        collect_yaml_files_from_dir(input, recursive, &mut files).await?;
    }

    Ok(files)
}

#[cfg(feature = "async")]
async fn collect_yaml_files_from_dir(
    dir: &PathBuf,
    recursive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();

        if path.is_file() && is_yaml_file(&path) {
            files.push(path);
        } else if path.is_dir() && recursive {
            Box::pin(collect_yaml_files_from_dir(&path, recursive, files)).await?;
        }
    }

    Ok(())
}

fn is_yaml_file(path: &PathBuf) -> bool {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        matches!(ext, "asset" | "prefab" | "unity" | "meta" | "yaml" | "yml")
    } else {
        false
    }
}

#[cfg(feature = "async")]
async fn extract_command_async(
    input: PathBuf,
    output: PathBuf,
    types: Vec<String>,
    recursive: bool,
    concurrency: usize,
    show_progress: bool,
) -> Result<()> {
    let start_time = Instant::now();

    println!("📂 Scanning for Unity files...");
    let unity_files = collect_yaml_files(&input, recursive).await?;

    if unity_files.is_empty() {
        println!("⚠️  No Unity files found in {:?}", input);
        return Ok(());
    }

    println!("📄 Found {} Unity files", unity_files.len());

    // Create output directory
    tokio::fs::create_dir_all(&output).await?;

    let progress = if show_progress {
        let pb = ProgressBar::new(unity_files.len() as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );
        Some(pb)
    } else {
        None
    };

    // Process files concurrently
    let results = stream::iter(unity_files)
        .map(|file_path| {
            let output = output.clone();
            let types = types.clone();
            let progress = progress.clone();
            async move {
                let result = extract_single_file(&file_path, &output, &types).await;
                if let Some(ref pb) = progress {
                    pb.inc(1);
                }
                (file_path, result)
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

    if let Some(pb) = progress {
        pb.finish_with_message("✅ Extraction complete");
    }

    // Report results
    let mut success_count = 0;
    let mut error_count = 0;
    let mut total_extracted = 0;

    for (file_path, result) in results {
        match result {
            Ok(extracted_count) => {
                success_count += 1;
                total_extracted += extracted_count;
                if !show_progress {
                    println!(
                        "✅ {}: {} entries extracted",
                        file_path.display(),
                        extracted_count
                    );
                }
            }
            Err(e) => {
                error_count += 1;
                eprintln!("❌ {}: {}", file_path.display(), e);
            }
        }
    }

    let elapsed = start_time.elapsed();
    println!("\n📊 Extraction Summary:");
    println!("  ✅ Files processed: {}", success_count);
    println!("  📦 Total entries extracted: {}", total_extracted);
    println!("  ❌ Errors: {}", error_count);
    println!("  ⏱️  Time: {:.2}s", elapsed.as_secs_f64());
    println!("  📁 Output directory: {}", output.display());

    Ok(())
}

#[cfg(feature = "async")]
async fn extract_single_file(
    file_path: &PathBuf,
    output_dir: &PathBuf,
    types: &[String],
) -> Result<usize> {
    let doc = unity_asset_yaml::YamlDocument::load_yaml_async(file_path, false).await?;

    // Filter by types if specified
    let entries_to_extract: Vec<_> = if types.is_empty() {
        UnityDocument::entries(&doc).iter().collect()
    } else {
        doc.filter(
            Some(&types.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
            None,
        )
    };

    let mut extracted_count = 0;

    // Extract each entry
    for (i, entry) in entries_to_extract.iter().enumerate() {
        let filename = format!("{}_{:03}_{}.yaml", entry.class_name, i, entry.anchor);
        let entry_path = output_dir.join(filename);

        // Create a single-entry document
        let mut single_doc = unity_asset_yaml::YamlDocument::new();
        single_doc.add_entry((*entry).clone());

        // Save the entry asynchronously
        single_doc.save_to_path_async(&entry_path).await?;
        extracted_count += 1;
    }

    Ok(extracted_count)
}
