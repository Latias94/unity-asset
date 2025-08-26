//! Unity Asset CLI V2
//!
//! Async command line tool for Unity assets.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use unity_asset_binary_v2::{AssetBundle, SerializedFile};
use unity_asset_core_v2::Result;
use unity_asset_yaml_v2::{YamlDocument, YamlLoader};

#[derive(Parser)]
#[command(name = "unity-asset-v2")]
#[command(about = "Unity Asset Parser V2 - Async version")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Parse a Unity YAML file asynchronously
    ParseYaml {
        /// Input YAML file path
        #[arg(short, long)]
        input: PathBuf,

        /// Output format (summary, detailed, json)
        #[arg(short, long, default_value = "summary")]
        format: String,

        /// Preserve original types instead of converting to strings
        #[arg(long)]
        preserve_types: bool,
    },

    /// Parse a Unity binary file asynchronously
    ParseBinary {
        /// Input binary file path (AssetBundle or SerializedFile)
        #[arg(short, long)]
        input: PathBuf,

        /// Output format (summary, detailed, json)
        #[arg(short, long, default_value = "summary")]
        format: String,
    },

    /// Extract information from Unity files asynchronously
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

        /// Maximum concurrent extractions
        #[arg(long, default_value = "8")]
        max_concurrent: usize,
    },

    /// Stream process large Unity files
    Stream {
        /// Input file path
        #[arg(short, long)]
        input: PathBuf,

        /// Processing mode (analyze, extract, convert)
        #[arg(short, long, default_value = "analyze")]
        mode: String,

        /// Buffer size for streaming (in KB)
        #[arg(long, default_value = "64")]
        buffer_size: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::ParseYaml {
            input,
            format,
            preserve_types,
        } => parse_yaml_command(input, format, preserve_types).await,
        Commands::ParseBinary { input, format } => parse_binary_command(input, format).await,
        Commands::Extract {
            input,
            output,
            types,
            max_concurrent,
        } => extract_command(input, output, types, max_concurrent).await,
        Commands::Stream {
            input,
            mode,
            buffer_size,
        } => stream_command(input, mode, buffer_size).await,
    }
}

/// Parse YAML file asynchronously (based on V1 implementation)
async fn parse_yaml_command(input: PathBuf, format: String, preserve_types: bool) -> Result<()> {
    println!("🔄 Parsing YAML file: {:?}", input);
    println!("📊 Output format: {}", format);
    println!("🔧 Preserve types: {}", preserve_types);

    // Load the YAML document asynchronously
    let doc = YamlDocument::load_from_path(&input).await?;

    println!("✅ Successfully loaded YAML document");
    println!("📦 Classes: {}", doc.classes().len());

    // Display entries based on format
    match format.as_str() {
        "summary" => {
            for (i, class) in doc.classes().iter().enumerate() {
                println!(
                    "  [{}]: {} (ID: {}, Anchor: {})",
                    i,
                    class.class_name(),
                    class.class_id,
                    class.anchor
                );
            }
        }
        "detailed" => {
            for (i, class) in doc.classes().iter().enumerate() {
                println!(
                    "  [{}]: {} (ID: {}, Anchor: {})",
                    i,
                    class.class_name(),
                    class.class_id,
                    class.anchor
                );
                let props = class.properties();
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
            println!("📄 JSON output not yet implemented in V2");
        }
        _ => {
            println!(
                "❌ Unknown format: {}. Supported formats: summary, detailed, json",
                format
            );
        }
    }

    Ok(())
}

/// Parse binary file asynchronously (new V2 feature)
async fn parse_binary_command(input: PathBuf, format: String) -> Result<()> {
    println!("🔄 Parsing binary file: {:?}", input);
    println!("📊 Output format: {}", format);

    let extension = input.extension().and_then(|s| s.to_str()).unwrap_or("");

    match extension {
        "bundle" | "unity3d" | "ab" => {
            // Parse as AssetBundle
            let bundle = AssetBundle::load_from_path(&input).await?;
            println!("✅ Successfully loaded AssetBundle");
            println!("📦 Assets: {}", bundle.assets.len());
            println!("🗂️ Files: {}", bundle.files.len());
            println!("📋 Blocks: {}", bundle.blocks.len());

            match format.as_str() {
                "summary" => {
                    println!("Bundle Header:");
                    println!("  Signature: {}", bundle.header.signature);
                    println!("  Version: {}", bundle.header.version);
                    println!("  Unity Version: {}", bundle.header.unity_version);
                }
                "detailed" => {
                    println!("Bundle Header:");
                    println!("  Signature: {}", bundle.header.signature);
                    println!("  Version: {}", bundle.header.version);
                    println!("  Unity Version: {}", bundle.header.unity_version);
                    println!("  Size: {} bytes", bundle.header.size);

                    println!("Files:");
                    for (i, file) in bundle.files.iter().enumerate() {
                        println!("  [{}]: {} ({} bytes)", i, file.name, file.size);
                    }
                }
                _ => println!("❌ Unknown format: {}", format),
            }
        }
        "assets" => {
            // Parse as SerializedFile
            let asset = SerializedFile::load_from_path(&input).await?;
            println!("✅ Successfully loaded SerializedFile");
            println!("📦 Objects: {}", asset.objects.len());
            println!("🏷️ Unity Version: {}", asset.unity_version);

            match format.as_str() {
                "summary" => {
                    println!("Asset Header:");
                    println!("  Version: {}", asset.header.version);
                    println!("  Platform: {}", asset.target_platform);
                }
                "detailed" => {
                    println!("Asset Header:");
                    println!("  Version: {}", asset.header.version);
                    println!("  Platform: {}", asset.target_platform);
                    println!("  Type Tree: {}", asset.enable_type_tree);

                    println!("Objects:");
                    for (i, obj) in asset.objects.iter().enumerate().take(10) {
                        println!(
                            "  [{}]: Class {} (Path ID: {})",
                            i, obj.class_id, obj.path_id
                        );
                    }
                    if asset.objects.len() > 10 {
                        println!("  ... and {} more objects", asset.objects.len() - 10);
                    }
                }
                _ => println!("❌ Unknown format: {}", format),
            }
        }
        _ => {
            println!("❌ Unsupported file type: {}", extension);
            println!("  Supported types: .bundle, .unity3d, .ab, .assets");
        }
    }

    Ok(())
}

/// Extract information from Unity files asynchronously (enhanced V2 version)
async fn extract_command(
    input: PathBuf,
    output: PathBuf,
    types: Vec<String>,
    max_concurrent: usize,
) -> Result<()> {
    println!("🔄 Extracting from: {:?}", input);
    println!("📁 Output to: {:?}", output);
    println!("🏷️ Types: {:?}", types);
    println!("⚡ Max concurrent: {}", max_concurrent);

    // Create output directory if it doesn't exist
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            unity_asset_core_v2::UnityAssetError::parse_error(
                format!("Failed to create output directory: {}", e),
                0,
            )
        })?;
    }

    let extension = input.extension().and_then(|s| s.to_str()).unwrap_or("");

    match extension {
        "asset" | "prefab" | "unity" | "meta" => {
            // Load as YAML document asynchronously
            let doc = YamlDocument::load_from_path(&input).await?;
            println!(
                "✅ Loaded YAML document with {} classes",
                doc.classes().len()
            );

            // Filter by types if specified
            let classes_to_extract: Vec<_> = if types.is_empty() {
                doc.classes().iter().collect()
            } else {
                doc.classes()
                    .iter()
                    .filter(|class| types.iter().any(|t| class.class_name() == t))
                    .collect()
            };

            println!("✅ Found {} classes to extract", classes_to_extract.len());

            // Use semaphore to limit concurrent extractions
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));
            let mut tasks = Vec::new();

            // Extract each class concurrently
            for (i, class) in classes_to_extract.iter().enumerate() {
                let class = (*class).clone();
                let output = output.clone();
                let semaphore = semaphore.clone();

                let task = tokio::spawn(async move {
                    let _permit = semaphore.acquire().await.unwrap();

                    let filename = format!("{}_{:03}_{}.yaml", class.class_name(), i, class.anchor);
                    let class_path = output.join(filename);

                    // Create a single-class document
                    let single_class_vec = vec![class];
                    let single_doc = YamlDocument::new(single_class_vec, Default::default());

                    // Serialize the class to YAML
                    let yaml_content = single_doc.serialize_to_yaml().await?;
                    tokio::fs::write(&class_path, yaml_content)
                        .await
                        .map_err(|e| {
                            unity_asset_core_v2::UnityAssetError::parse_error(
                                format!("Failed to write file: {}", e),
                                0,
                            )
                        })?;

                    println!("  ✅ Extracted: {}", class_path.display());
                    Ok::<(), unity_asset_core_v2::UnityAssetError>(())
                });

                tasks.push(task);
            }

            // Wait for all extractions to complete
            for task in tasks {
                task.await.map_err(|e| {
                    unity_asset_core_v2::UnityAssetError::parse_error(
                        format!("Task failed: {}", e),
                        0,
                    )
                })??;
            }
        }
        "bundle" | "unity3d" | "ab" => {
            // Load as AssetBundle asynchronously
            let bundle = AssetBundle::load_from_path(&input).await?;
            println!("✅ Loaded AssetBundle with {} assets", bundle.assets.len());

            // Extract assets concurrently
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));
            let mut tasks = Vec::new();

            for (i, asset) in bundle.assets.iter().enumerate() {
                let asset = asset.clone(); // Assuming Clone is implemented
                let output = output.clone();
                let semaphore = semaphore.clone();

                let task = tokio::spawn(async move {
                    let _permit = semaphore.acquire().await.unwrap();

                    let filename = format!("asset_{:03}.assets", i);
                    let asset_path = output.join(filename);

                    // For now, just create a placeholder file with asset info
                    let info_content = format!(
                        "Asset Info:\nUnity Version: {}\nObjects: {}\nPlatform: {}\n",
                        asset.unity_version,
                        asset.objects.len(),
                        asset.target_platform
                    );

                    tokio::fs::write(&asset_path, info_content)
                        .await
                        .map_err(|e| {
                            unity_asset_core_v2::UnityAssetError::parse_error(
                                format!("Failed to write asset info: {}", e),
                                0,
                            )
                        })?;

                    println!("  ✅ Extracted asset info: {}", asset_path.display());
                    Ok::<(), unity_asset_core_v2::UnityAssetError>(())
                });

                tasks.push(task);
            }

            // Wait for all extractions to complete
            for task in tasks {
                task.await.map_err(|e| {
                    unity_asset_core_v2::UnityAssetError::parse_error(
                        format!("Task failed: {}", e),
                        0,
                    )
                })??;
            }
        }
        _ => {
            println!("❌ Unsupported file type: {}", extension);
            println!("  Supported types: .asset, .prefab, .unity, .meta, .bundle, .unity3d, .ab");
        }
    }

    Ok(())
}

/// Stream process large Unity files (V2 exclusive feature)
async fn stream_command(input: PathBuf, mode: String, buffer_size: usize) -> Result<()> {
    println!("🌊 Stream processing: {:?}", input);
    println!("🔧 Mode: {}", mode);
    println!("📦 Buffer size: {} KB", buffer_size);

    let buffer_bytes = buffer_size * 1024;

    match mode.as_str() {
        "analyze" => {
            println!("🔍 Analyzing file structure...");

            // Get file size
            let metadata = tokio::fs::metadata(&input).await.map_err(|e| {
                unity_asset_core_v2::UnityAssetError::parse_error(
                    format!("Failed to read file metadata: {}", e),
                    0,
                )
            })?;

            println!(
                "📊 File size: {} bytes ({:.2} MB)",
                metadata.len(),
                metadata.len() as f64 / 1024.0 / 1024.0
            );

            let extension = input.extension().and_then(|s| s.to_str()).unwrap_or("");

            match extension {
                "bundle" | "unity3d" | "ab" => {
                    println!("🎯 Detected: AssetBundle");
                    println!("📊 Performing basic header analysis...");

                    // Read first few bytes to analyze header
                    let file = tokio::fs::File::open(&input).await.map_err(|e| {
                        unity_asset_core_v2::UnityAssetError::parse_error(
                            format!("Failed to open file: {}", e),
                            0,
                        )
                    })?;

                    let mut reader = tokio::io::BufReader::with_capacity(buffer_bytes, file);
                    let mut header_buffer = vec![0u8; 64]; // Read first 64 bytes

                    use tokio::io::AsyncReadExt;
                    let bytes_read = reader.read(&mut header_buffer).await.map_err(|e| {
                        unity_asset_core_v2::UnityAssetError::parse_error(
                            format!("Failed to read header: {}", e),
                            0,
                        )
                    })?;

                    if bytes_read >= 8 {
                        let signature = String::from_utf8_lossy(&header_buffer[0..8]);
                        println!("  Signature: {}", signature.trim_end_matches('\0'));

                        if bytes_read >= 12 {
                            let version = u32::from_be_bytes([
                                header_buffer[8],
                                header_buffer[9],
                                header_buffer[10],
                                header_buffer[11],
                            ]);
                            println!("  Version: {}", version);
                        }
                    }
                }
                "assets" => {
                    println!("🎯 Detected: SerializedFile");
                    println!("📊 Performing basic header analysis...");

                    // Similar basic analysis for SerializedFile
                    let file = tokio::fs::File::open(&input).await.map_err(|e| {
                        unity_asset_core_v2::UnityAssetError::parse_error(
                            format!("Failed to open file: {}", e),
                            0,
                        )
                    })?;

                    let mut reader = tokio::io::BufReader::with_capacity(buffer_bytes, file);
                    let mut header_buffer = vec![0u8; 32];

                    use tokio::io::AsyncReadExt;
                    let bytes_read = reader.read(&mut header_buffer).await.map_err(|e| {
                        unity_asset_core_v2::UnityAssetError::parse_error(
                            format!("Failed to read header: {}", e),
                            0,
                        )
                    })?;

                    if bytes_read >= 20 {
                        let metadata_size = u32::from_le_bytes([
                            header_buffer[0],
                            header_buffer[1],
                            header_buffer[2],
                            header_buffer[3],
                        ]);
                        let file_size = u32::from_le_bytes([
                            header_buffer[4],
                            header_buffer[5],
                            header_buffer[6],
                            header_buffer[7],
                        ]);
                        let version = u32::from_le_bytes([
                            header_buffer[8],
                            header_buffer[9],
                            header_buffer[10],
                            header_buffer[11],
                        ]);

                        println!("  Metadata size: {} bytes", metadata_size);
                        println!("  File size: {} bytes", file_size);
                        println!("  Format version: {}", version);
                    }
                }
                "asset" | "prefab" | "unity" | "meta" => {
                    println!("🎯 Detected: YAML file");
                    // For YAML files, we can do streaming line analysis
                    let mut line_count = 0;
                    let mut document_count = 0;
                    let mut class_count = 0;

                    let file = tokio::fs::File::open(&input).await.map_err(|e| {
                        unity_asset_core_v2::UnityAssetError::parse_error(
                            format!("Failed to open file: {}", e),
                            0,
                        )
                    })?;

                    let mut reader = tokio::io::BufReader::with_capacity(buffer_bytes, file);
                    let mut line = String::new();

                    loop {
                        line.clear();
                        let bytes_read =
                            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                                .await
                                .map_err(|e| {
                                    unity_asset_core_v2::UnityAssetError::parse_error(
                                        format!("Failed to read line: {}", e),
                                        0,
                                    )
                                })?;

                        if bytes_read == 0 {
                            break; // EOF
                        }

                        line_count += 1;

                        if line.trim().starts_with("---") {
                            document_count += 1;
                        }

                        if line.trim().ends_with(':') && !line.trim().starts_with(' ') {
                            class_count += 1;
                        }

                        // Progress indicator for large files
                        if line_count % 10000 == 0 {
                            println!("📈 Processed {} lines...", line_count);
                        }
                    }

                    println!("📊 Analysis complete:");
                    println!("  Lines: {}", line_count);
                    println!("  Documents: {}", document_count);
                    println!("  Estimated classes: {}", class_count);
                }
                _ => {
                    println!("❌ Unsupported file type for streaming: {}", extension);
                }
            }
        }
        "extract" => {
            println!("📤 Stream extraction mode");
            println!("⚠️  This would implement streaming extraction for large files");
            println!("    - Extract objects without loading entire file into memory");
            println!("    - Process objects in chunks with configurable buffer size");
            println!("    - Support for concurrent extraction of multiple objects");
        }
        "convert" => {
            println!("🔄 Stream conversion mode");
            println!("⚠️  This would implement streaming format conversion");
            println!("    - Convert between Unity formats (YAML ↔ Binary)");
            println!("    - Stream processing for memory efficiency");
            println!("    - Preserve all metadata during conversion");
        }
        _ => {
            println!(
                "❌ Unknown mode: {}. Supported modes: analyze, extract, convert",
                mode
            );
        }
    }

    Ok(())
}
