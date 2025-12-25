//! Unity Asset Parser CLI
//!
//! Command-line interface for parsing and manipulating Unity assets.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use unity_asset::UnityDocument;
use unity_asset::UnityValue;
use unity_asset::environment::{
    BinaryObjectKey, BinarySource, Environment, EnvironmentOptions, EnvironmentReporter,
    EnvironmentWarning,
};
use unity_asset_binary::{asset::class_ids, object::UnityObject, unity_version::UnityVersion};

#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipConverter, AudioProcessor},
    sprite::SpriteProcessor,
    texture::{TextureExporter, TextureProcessor},
};

#[derive(Parser)]
#[command(name = "unity_asset")]
#[command(about = "A Rust-based Unity asset parser")]
#[command(version)]
struct Cli {
    /// Fail-fast TypeTree parsing (no best-effort fallbacks)
    #[arg(long)]
    strict: bool,

    /// Print collected load warnings and TypeTree warnings (when applicable)
    #[arg(long)]
    show_warnings: bool,

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

    /// Export objects from AssetBundles using the bundle `m_Container` (UnityPy-like workflow)
    ExportBundle {
        /// Input file or directory path (bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Output directory
        #[arg(short, long)]
        output: PathBuf,

        /// Filter container entries by substring (case-insensitive). Empty means export all.
        #[arg(long, default_value = "")]
        pattern: String,

        /// Limit exported entries (to keep runtime predictable)
        #[arg(long)]
        limit: Option<usize>,

        /// Only print what would be exported
        #[arg(long)]
        dry_run: bool,

        /// Decode known types (AudioClip -> WAV, Texture2D -> PNG) instead of exporting raw object bytes
        #[arg(long)]
        decode: bool,
    },

    /// List AssetBundle nodes (files) for debugging and inspection
    ListBundle {
        /// Input AssetBundle path
        #[arg(short, long)]
        input: PathBuf,

        /// Filter node names by substring (case-insensitive). Empty means show all.
        #[arg(long, default_value = "")]
        filter: String,

        /// Print offsets and sizes
        #[arg(long)]
        verbose: bool,
    },

    /// Find objects by AssetBundle `m_Container` asset path pattern (UnityPy-like discovery)
    FindObject {
        /// Input file or directory path (bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Filter container entries by substring (case-insensitive). Empty means show all.
        #[arg(long, default_value = "")]
        pattern: String,

        /// Filter by Unity class ID (repeatable). Example: `--class-id 83` (AudioClip).
        #[arg(long)]
        class_id: Vec<i32>,

        /// Filter by Unity class name substring (case-insensitive). Example: `--class-name Texture`.
        #[arg(long, default_value = "")]
        class_name: String,

        /// Limit matched entries
        #[arg(long)]
        limit: Option<usize>,

        /// Include entries that could not be resolved to a `BinaryObjectKey`
        #[arg(long)]
        include_unresolved: bool,

        /// Print extra object info (type_id, byte_size) when resolvable
        #[arg(long)]
        verbose: bool,
    },

    /// Inspect a single object by source location (useful for TypeTree debugging)
    InspectObject {
        /// Input file or directory path (assets/bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Copy/paste key emitted by `find-object --verbose` (overrides --source/--kind/--asset-index/--path-id)
        #[arg(long)]
        key: Option<String>,

        /// Source file path that contains the object (an AssetBundle or a standalone SerializedFile)
        #[arg(long)]
        source: Option<PathBuf>,

        /// Source kind: `bundle` or `serialized`
        #[arg(long, default_value = "bundle")]
        kind: String,

        /// Asset index inside the bundle (required when `--kind bundle`)
        #[arg(long)]
        asset_index: Option<usize>,

        /// Object PathID inside the serialized file
        #[arg(long)]
        path_id: Option<i64>,

        /// Limit printed recursion depth
        #[arg(long, default_value_t = 6)]
        max_depth: usize,

        /// Limit total printed nodes (prevents huge dumps)
        #[arg(long, default_value_t = 500)]
        max_items: usize,

        /// Limit printed array items per array node
        #[arg(long, default_value_t = 16)]
        max_array: usize,

        /// Only print paths containing this substring (case-insensitive)
        #[arg(long, default_value = "")]
        filter: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let strict = cli.strict;
    let show_warnings = cli.show_warnings;

    match cli.command {
        Commands::ParseYaml {
            input,
            format,
            preserve_types,
        } => parse_yaml_command(input, format, preserve_types, show_warnings),
        Commands::Extract {
            input,
            output,
            types,
        } => extract_command(input, output, types),
        Commands::ExportBundle {
            input,
            output,
            pattern,
            limit,
            dry_run,
            decode,
        } => export_bundle_command(
            input,
            output,
            pattern,
            limit,
            dry_run,
            decode,
            strict,
            show_warnings,
        ),
        Commands::ListBundle {
            input,
            filter,
            verbose,
        } => list_bundle_command(input, filter, verbose, strict, show_warnings),
        Commands::FindObject {
            input,
            pattern,
            class_id,
            class_name,
            limit,
            include_unresolved,
            verbose,
        } => find_object_command(
            input,
            pattern,
            class_id,
            class_name,
            limit,
            include_unresolved,
            verbose,
            strict,
            show_warnings,
        ),
        Commands::InspectObject {
            input,
            key,
            source,
            kind,
            asset_index,
            path_id,
            max_depth,
            max_items,
            max_array,
            filter,
        } => inspect_object_command(
            input,
            key,
            source,
            kind,
            asset_index,
            path_id,
            max_depth,
            max_items,
            max_array,
            filter,
            strict,
            show_warnings,
        ),
    }
}

#[derive(Debug)]
struct CliReporter {
    enabled: bool,
}

impl EnvironmentReporter for CliReporter {
    fn warn(&self, warning: &EnvironmentWarning) {
        if !self.enabled {
            return;
        }
        eprintln!("warning: {}", warning);
    }

    fn typetree_warning(
        &self,
        key: &BinaryObjectKey,
        warning: &unity_asset_binary::typetree::TypeTreeParseWarning,
    ) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "warning: typetree key={} field={} error={}",
            key, warning.field, warning.error
        );
    }
}

fn build_environment(strict: bool, show_warnings: bool) -> Environment {
    let mut env = if strict {
        Environment::with_options(EnvironmentOptions::strict())
    } else {
        Environment::new()
    };

    let reporter: Option<Arc<dyn EnvironmentReporter>> = if show_warnings {
        Some(Arc::new(CliReporter { enabled: true }))
    } else {
        None
    };
    env.set_reporter(reporter);
    env
}

fn parse_yaml_command(
    input: PathBuf,
    format: String,
    preserve_types: bool,
    show_warnings: bool,
) -> Result<()> {
    println!("Parsing YAML file: {:?}", input);
    println!("Output format: {}", format);
    println!("Preserve types: {}", preserve_types);

    // Load the YAML document
    let (doc, warnings) =
        unity_asset::YamlDocument::load_yaml_with_warnings(&input, preserve_types)?;
    if show_warnings {
        for w in warnings {
            eprintln!("warning: {}", w);
        }
    }

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

fn sanitize_asset_path(asset_path: &str) -> PathBuf {
    let normalized = asset_path.trim_start_matches('/').replace('\\', "/");
    let mut out = PathBuf::new();

    for comp in normalized.split('/').filter(|s| !s.is_empty()) {
        let mut clean = String::with_capacity(comp.len());
        for ch in comp.chars() {
            let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | ' ');
            clean.push(if keep { ch } else { '_' });
        }
        if clean.is_empty() || clean == "." || clean == ".." {
            clean = format!(
                "_{}_",
                if clean.is_empty() {
                    "empty"
                } else {
                    clean.as_str()
                }
            );
        }
        out.push(clean);
    }

    out
}

fn magic_based_extension(asset_path: &str, bytes: &[u8]) -> Option<&'static str> {
    let ext = std::path::Path::new(asset_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())?;

    match ext.as_str() {
        "ogg" if bytes.len() >= 4 && &bytes[0..4] == b"OggS" => Some("ogg"),
        "png" if bytes.len() >= 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" => Some("png"),
        "jpg" | "jpeg" if bytes.len() >= 3 && &bytes[0..3] == b"\xFF\xD8\xFF" => Some("jpg"),
        "wav" if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" => {
            Some("wav")
        }
        _ => None,
    }
}

fn text_asset_bytes(obj: &UnityObject) -> Vec<u8> {
    // Unity TextAsset commonly stores either:
    // - `m_Script` (string)
    // - `m_Text` (string, seen in some variants)
    // - `m_Bytes` (byte array)
    // We'll prefer text fields first to preserve encoding.
    if let Some(UnityValue::String(s)) = obj.get("m_Script") {
        if !s.is_empty() {
            return s.as_bytes().to_vec();
        }
    }

    if let Some(UnityValue::String(s)) = obj.get("m_Text") {
        if !s.is_empty() {
            return s.as_bytes().to_vec();
        }
    }

    for key in ["m_Bytes", "m_Data"] {
        let Some(UnityValue::Array(arr)) = obj.get(key) else {
            continue;
        };
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            match v {
                UnityValue::Integer(i) if (0..=255).contains(i) => out.push(*i as u8),
                _ => return Vec::new(),
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    Vec::new()
}

fn sprite_texture_pptr(obj: &UnityObject) -> Option<(i32, i64)> {
    let UnityValue::Object(rd) = obj.get("m_RD")? else {
        return None;
    };
    let UnityValue::Object(texture) = rd.get("texture")? else {
        return None;
    };
    let file_id = match texture.get("m_FileID")? {
        UnityValue::Integer(v) => *v as i32,
        _ => return None,
    };
    let path_id = match texture.get("m_PathID")? {
        UnityValue::Integer(v) => *v,
        _ => return None,
    };
    Some((file_id, path_id))
}

fn export_bundle_command(
    input: PathBuf,
    output: PathBuf,
    pattern: String,
    limit: Option<usize>,
    dry_run: bool,
    decode: bool,
    strict: bool,
    show_warnings: bool,
) -> Result<()> {
    let mut env = build_environment(strict, show_warnings);
    env.load(&input)?;

    std::fs::create_dir_all(&output)?;

    let mut bundle_sources: Vec<BinarySource> = env
        .binary_sources()
        .into_iter()
        .filter_map(|(kind, s)| {
            if kind == unity_asset::environment::BinarySourceKind::AssetBundle {
                Some(s)
            } else {
                None
            }
        })
        .collect();
    bundle_sources.sort();

    if bundle_sources.is_empty() {
        println!("⚠ No AssetBundles found in {:?}", input);
        return Ok(());
    }

    let mut exported = 0usize;
    let mut skipped = 0usize;
    let pattern_lc = pattern.to_ascii_lowercase();

    for bundle_source in bundle_sources {
        let entries = env.bundle_container_entries_source(&bundle_source)?;
        let mut entries: Vec<_> = entries
            .into_iter()
            .filter(|e| e.asset_path.to_ascii_lowercase().contains(&pattern_lc))
            .collect();
        entries.sort_by(|a, b| a.asset_path.cmp(&b.asset_path));

        for entry in entries {
            if let Some(max) = limit {
                if exported >= max {
                    break;
                }
            }

            let Some(key) = entry.key else {
                skipped += 1;
                continue;
            };

            let rel = sanitize_asset_path(&entry.asset_path);
            let mut dest_raw = output.join(rel);
            dest_raw.set_extension("bin");

            let disambiguate = |mut dest: PathBuf| -> PathBuf {
                if dest.exists() {
                    let mut i = 1usize;
                    loop {
                        let mut alt = dest.clone();
                        let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or("bin");
                        alt.set_extension(format!("{}.{}", i, ext));
                        if !alt.exists() {
                            dest = alt;
                            break;
                        }
                        i += 1;
                    }
                }
                dest
            };

            if dry_run {
                let mut dest = dest_raw.clone();
                if decode {
                    // Best-effort preview: keep original extension if present; decoding is decided at runtime.
                    dest = output.join(sanitize_asset_path(&entry.asset_path));
                    if dest.extension().is_none() {
                        dest.set_extension("bin");
                    }
                }
                dest = disambiguate(dest);
                println!("DRY-RUN {} -> {:?}", entry.asset_path, dest);
                exported += 1;
                continue;
            }

            let obj = env.read_binary_object_key(&key)?;

            if decode {
                #[cfg(not(feature = "decode"))]
                {
                    let _ = obj;
                    anyhow::bail!(
                        "--decode requires compiling `unity-asset-cli` with feature `decode` (build with default features, or `--features decode`)."
                    );
                }

                #[cfg(feature = "decode")]
                let unity_version = match key.source_kind {
                    unity_asset::environment::BinarySourceKind::AssetBundle => env
                        .bundles()
                        .get(&key.source)
                        .and_then(|b| key.asset_index.and_then(|i| b.assets.get(i)))
                        .map(|f| UnityVersion::parse_version(&f.unity_version).unwrap_or_default())
                        .unwrap_or_default(),
                    unity_asset::environment::BinarySourceKind::SerializedFile => env
                        .binary_assets()
                        .get(&key.source)
                        .map(|f| UnityVersion::parse_version(&f.unity_version).unwrap_or_default())
                        .unwrap_or_default(),
                };

                #[cfg(feature = "decode")]
                let decoded_path: Option<PathBuf> = match obj.info.type_id {
                    class_ids::AUDIO_CLIP => (|| -> anyhow::Result<Option<PathBuf>> {
                        let converter = AudioClipConverter::new(unity_version.clone());
                        let clip = converter.from_unity_object(&obj)?;

                        if std::env::var_os("UNITY_ASSET_DEBUG_AUDIO").is_some() {
                            eprintln!(
                                "AudioClip debug: name={:?}, data_len={}, is_streamed={}, stream_path={:?}, stream_offset={}, stream_size={}",
                                clip.name,
                                clip.data.len(),
                                clip.is_streamed(),
                                clip.stream_info.path,
                                clip.stream_info.offset,
                                clip.stream_info.size,
                            );
                            if let Some(UnityValue::Object(res)) = obj.get("m_Resource") {
                                eprintln!("  m_Resource keys: {:?}", res.keys().collect::<Vec<_>>());
                                eprintln!("  m_Resource: {:?}", res);
                            }
                            if let Some(UnityValue::Array(items)) = obj.get("m_AudioData") {
                                eprintln!("  m_AudioData len: {}", items.len());
                            }
                            eprintln!("  m_CompressionFormat: {:?}", obj.get("m_CompressionFormat"));
                            eprintln!("  m_LoadType: {:?}", obj.get("m_LoadType"));
                            eprintln!("  m_Channels: {:?}", obj.get("m_Channels"));
                            eprintln!("  m_Frequency: {:?}", obj.get("m_Frequency"));
                            eprintln!("  m_BitsPerSample: {:?}", obj.get("m_BitsPerSample"));
                            eprintln!("  m_Length: {:?}", obj.get("m_Length"));

                            if clip.is_streamed()
                                && key.source_kind == unity_asset::environment::BinarySourceKind::AssetBundle
                            {
                                match env.read_bundle_stream_data_source(
                                    &key.source,
                                    &clip.stream_info.path,
                                    clip.stream_info.offset,
                                    clip.stream_info.size,
                                ) {
                                    Ok(bytes) => eprintln!("  bundle stream bytes: {}", bytes.len()),
                                    Err(e) => eprintln!("  bundle stream error: {}", e),
                                }
                            }
                        }

                        let mut dest = output.join(sanitize_asset_path(&entry.asset_path));
                        match converter.get_audio_data(&clip) {
                            Ok(audio_bytes) if !audio_bytes.is_empty() => {
                                let ext = std::path::Path::new(&entry.asset_path)
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .unwrap_or(clip.compression_format().extension())
                                    .to_ascii_lowercase();
                                dest.set_extension(ext);
                                dest = disambiguate(dest);
                                if let Some(parent) = dest.parent() {
                                    std::fs::create_dir_all(parent)?;
                                }
                                std::fs::write(&dest, &audio_bytes)?;
                                Ok(Some(dest))
                            }
                            _ => {
                                // If the clip is streamed and we are exporting from a bundle, try to read
                                // the resource bytes from the bundle or filesystem (UnityPy-like).
                                if clip.is_streamed() {
                                    if let Ok(bytes) = env.read_stream_data_source(
                                        &key.source,
                                        key.source_kind,
                                        &clip.stream_info.path,
                                        clip.stream_info.offset,
                                        clip.stream_info.size,
                                    ) {
                                        if !bytes.is_empty() {
                                            let ext = std::path::Path::new(&entry.asset_path)
                                                .extension()
                                                .and_then(|e| e.to_str())
                                                .unwrap_or(clip.compression_format().extension())
                                                .to_ascii_lowercase();
                                            dest.set_extension(ext);
                                            dest = disambiguate(dest);
                                            if let Some(parent) = dest.parent() {
                                                std::fs::create_dir_all(parent)?;
                                            }
                                            std::fs::write(&dest, &bytes)?;
                                            return Ok(Some(dest));
                                        }
                                    }
                                }

                                dest.set_extension("wav");
                                dest = disambiguate(dest);
                                if let Some(parent) = dest.parent() {
                                    std::fs::create_dir_all(parent)?;
                                }
                                let audio_processor = AudioProcessor::new(unity_version);
                                audio_processor.process_and_export(&obj, &dest)?;
                                Ok(Some(dest))
                            }
                        }
                    })()
                    .ok()
                    .flatten(),
                    class_ids::TEXTURE_2D => (|| -> anyhow::Result<Option<PathBuf>> {
                        let mut dest = output.join(sanitize_asset_path(&entry.asset_path));
                        dest.set_extension("png");
                        dest = disambiguate(dest);
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent)?;
                        }

                        let texture_processor = TextureProcessor::new(unity_version);
                        let mut texture = texture_processor.convert_object(&obj)?;
                        if texture.image_data.is_empty() && texture.is_streamed() {
                            if let Ok(bytes) = env.read_stream_data_source(
                                &key.source,
                                key.source_kind,
                                &texture.stream_info.path,
                                texture.stream_info.offset,
                                texture.stream_info.size,
                            ) {
                                if !bytes.is_empty() {
                                    texture.data_size = bytes.len() as i32;
                                    texture.image_data = bytes;
                                }
                            }
                        }

                        let image = texture_processor.decode_texture(&texture)?;
                        TextureExporter::export_auto(&image, &dest)?;
                        Ok(Some(dest))
                    })()
                    .ok()
                    .flatten(),
                    class_ids::TEXT_ASSET => (|| -> anyhow::Result<Option<PathBuf>> {
                        let bytes = text_asset_bytes(&obj);
                        if bytes.is_empty() {
                            return Ok(None);
                        }

                        let mut dest = output.join(sanitize_asset_path(&entry.asset_path));
                        if dest.extension().is_none() {
                            dest.set_extension(if std::str::from_utf8(&bytes).is_ok() {
                                "txt"
                            } else {
                                "bin"
                            });
                        }
                        dest = disambiguate(dest);
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&dest, &bytes)?;
                        Ok(Some(dest))
                    })()
                    .ok()
                    .flatten(),
                    class_ids::SPRITE => (|| -> anyhow::Result<Option<PathBuf>> {
                        let Some(obj_ref) = (match key.source_kind {
                            unity_asset::environment::BinarySourceKind::AssetBundle => key
                                .asset_index
                                .and_then(|i| env.find_binary_object_in_bundle_asset_source(&key.source, i, key.path_id)),
                            unity_asset::environment::BinarySourceKind::SerializedFile => {
                                env.find_binary_object_in_source_id(&key.source, key.path_id)
                            }
                        }) else {
                            return Ok(None);
                        };

                        let sprite_processor = SpriteProcessor::new(unity_version.clone());
                        let sprite = sprite_processor.parse_sprite(&obj)?.sprite;

                        let (file_id, texture_path_id) = if let Some((file_id, path_id)) = sprite_texture_pptr(&obj) {
                            (file_id, path_id)
                        } else if sprite.render_data.texture_path_id != 0 {
                            (0, sprite.render_data.texture_path_id)
                        } else {
                            return Ok(None);
                        };

                        let texture_obj = env.read_binary_pptr(&obj_ref, file_id, texture_path_id)?;

                        let texture_processor = TextureProcessor::new(unity_version);
                        let mut texture = texture_processor.convert_object(&texture_obj)?;
                        if texture.image_data.is_empty() && texture.is_streamed() {
                            if let Ok(bytes) = env.read_stream_data_source(
                                &key.source,
                                key.source_kind,
                                &texture.stream_info.path,
                                texture.stream_info.offset,
                                texture.stream_info.size,
                            ) {
                                if !bytes.is_empty() {
                                    texture.data_size = bytes.len() as i32;
                                    texture.image_data = bytes;
                                }
                            }
                        }

                        let png_bytes = sprite_processor.extract_sprite_image(&sprite, &texture)?;

                        let mut dest = output.join(sanitize_asset_path(&entry.asset_path));
                        if dest.extension().is_some() {
                            let stem = dest
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("sprite");
                            dest.set_file_name(format!("{}.sprite.png", stem));
                        } else {
                            dest.set_extension("png");
                        }
                        dest = disambiguate(dest);
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&dest, &png_bytes)?;
                        Ok(Some(dest))
                    })()
                    .ok()
                    .flatten(),
                    _ => None,
                };

                #[cfg(feature = "decode")]
                if let Some(dest) = decoded_path {
                    println!(
                        "✓ {} -> {:?} (decoded, class_id={})",
                        entry.asset_path, dest, obj.info.type_id
                    );
                    exported += 1;
                    continue;
                }
            }

            let mut dest = dest_raw;
            if decode {
                // If decoding didn't apply, still try to preserve the original extension
                // when the raw bytes match the expected file signature (UnityPy-like behavior for TextAsset, etc.).
                let bytes = obj.raw_data();
                if let Some(ext) = magic_based_extension(&entry.asset_path, bytes) {
                    dest = output.join(sanitize_asset_path(&entry.asset_path));
                    dest.set_extension(ext);
                } else if dest.extension().is_none() {
                    dest.set_extension("bin");
                }
            }
            let dest = disambiguate(dest);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = obj.raw_data();
            std::fs::write(&dest, bytes)?;
            println!(
                "✓ {} -> {:?} (raw, class_id={}, bytes={})",
                entry.asset_path,
                dest,
                obj.info.type_id,
                bytes.len()
            );
            exported += 1;
        }
    }

    println!(
        "Exported {} entries, skipped {} (unresolved PPtr/external refs or missing objects)",
        exported, skipped
    );
    Ok(())
}

fn list_bundle_command(
    input: PathBuf,
    filter: String,
    verbose: bool,
    strict: bool,
    show_warnings: bool,
) -> Result<()> {
    let mut env = build_environment(strict, show_warnings);
    env.load(&input)?;

    let filter_lc = filter.to_ascii_lowercase();
    let mut bundle_sources: Vec<BinarySource> = env
        .binary_sources()
        .into_iter()
        .filter_map(|(kind, s)| {
            if kind == unity_asset::environment::BinarySourceKind::AssetBundle {
                Some(s)
            } else {
                None
            }
        })
        .collect();
    bundle_sources.sort();

    if bundle_sources.is_empty() {
        println!("⚠ No AssetBundles found in {:?}", input);
        return Ok(());
    }

    for bundle_source in bundle_sources {
        let Some(bundle) = env.bundles().get(&bundle_source) else {
            continue;
        };

        println!(
            "Bundle: {} (nodes={}, assets={})",
            bundle_source,
            bundle.nodes.len(),
            bundle.assets.len()
        );

        let mut nodes: Vec<_> = bundle.nodes.iter().filter(|n| n.is_file()).collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        for node in nodes {
            if !filter_lc.is_empty() && !node.name.to_ascii_lowercase().contains(&filter_lc) {
                continue;
            }
            if verbose {
                println!(
                    "  - {} (offset={}, size={}, flags={})",
                    node.name, node.offset, node.size, node.flags
                );
            } else {
                println!("  - {}", node.name);
            }
        }
    }

    Ok(())
}

fn find_object_command(
    input: PathBuf,
    pattern: String,
    class_id: Vec<i32>,
    class_name: String,
    limit: Option<usize>,
    include_unresolved: bool,
    verbose: bool,
    strict: bool,
    show_warnings: bool,
) -> Result<()> {
    let mut env = build_environment(strict, show_warnings);
    env.load(&input)?;

    let pattern_lc = pattern.to_ascii_lowercase();
    let class_name_lc = class_name.to_ascii_lowercase();
    let class_ids = class_id;

    let mut bundle_sources: Vec<BinarySource> = env
        .binary_sources()
        .into_iter()
        .filter_map(|(kind, s)| {
            if kind == unity_asset::environment::BinarySourceKind::AssetBundle {
                Some(s)
            } else {
                None
            }
        })
        .collect();
    bundle_sources.sort();

    if bundle_sources.is_empty() {
        println!("⚠ No AssetBundles found in {:?}", input);
        return Ok(());
    }

    let mut count = 0usize;
    for bundle_source in bundle_sources {
        let mut entries = env.bundle_container_entries_source(&bundle_source)?;
        entries.sort_by(|a, b| a.asset_path.cmp(&b.asset_path));

        for entry in entries {
            if let Some(max) = limit {
                if count >= max {
                    return Ok(());
                }
            }

            if !pattern_lc.is_empty()
                && !entry.asset_path.to_ascii_lowercase().contains(&pattern_lc)
            {
                continue;
            }

            if entry.key.is_none()
                && (!include_unresolved || !class_ids.is_empty() || !class_name_lc.is_empty())
            {
                continue;
            }

            if verbose {
                if let Some(key) = &entry.key {
                    let (type_id, byte_size) = lookup_object_type_info(&env, key);

                    if !class_ids.is_empty() && !class_ids.contains(&type_id) {
                        continue;
                    }
                    if !class_name_lc.is_empty() {
                        let name = unity_asset::get_class_name(type_id)
                            .unwrap_or_else(|| format!("Class_{}", type_id));
                        if !name.to_ascii_lowercase().contains(&class_name_lc) {
                            continue;
                        }
                    }

                    println!(
                        "{} -> key={} type_id={} byte_size={}",
                        entry.asset_path, key, type_id, byte_size
                    );
                } else {
                    println!(
                        "{} -> unresolved(bundle={}, asset_index={}, file_id={}, path_id={})",
                        entry.asset_path,
                        entry.bundle_source,
                        entry.asset_index,
                        entry.file_id,
                        entry.path_id
                    );
                }
            } else if let Some(key) = &entry.key {
                let (type_id, _byte_size) = if class_ids.is_empty() && class_name_lc.is_empty() {
                    (0, 0)
                } else {
                    lookup_object_type_info(&env, key)
                };
                if !class_ids.is_empty() && !class_ids.contains(&type_id) {
                    continue;
                }
                if !class_name_lc.is_empty() {
                    let name = unity_asset::get_class_name(type_id)
                        .unwrap_or_else(|| format!("Class_{}", type_id));
                    if !name.to_ascii_lowercase().contains(&class_name_lc) {
                        continue;
                    }
                }
                println!("{} -> key={}", entry.asset_path, key);
            } else {
                println!("{} -> unresolved", entry.asset_path);
            }

            count += 1;
        }
    }

    Ok(())
}

fn lookup_object_type_info(
    env: &Environment,
    key: &unity_asset::environment::BinaryObjectKey,
) -> (i32, u32) {
    match key.source_kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => env
            .bundles()
            .get(&key.source)
            .and_then(|b| key.asset_index.and_then(|i| b.assets.get(i)))
            .and_then(|f| f.find_object(key.path_id))
            .map(|info| (info.type_id, info.byte_size))
            .unwrap_or((0, 0)),
        unity_asset::environment::BinarySourceKind::SerializedFile => env
            .binary_assets()
            .get(&key.source)
            .and_then(|f| f.find_object(key.path_id))
            .map(|info| (info.type_id, info.byte_size))
            .unwrap_or((0, 0)),
    }
}

fn inspect_object_command(
    input: PathBuf,
    key: Option<String>,
    source: Option<PathBuf>,
    kind: String,
    asset_index: Option<usize>,
    path_id: Option<i64>,
    max_depth: usize,
    max_items: usize,
    max_array: usize,
    filter: String,
    strict: bool,
    show_warnings: bool,
) -> Result<()> {
    let mut env = build_environment(strict, show_warnings);
    env.load(&input)?;

    let mut key = if let Some(key) = key {
        key.parse::<unity_asset::environment::BinaryObjectKey>()
            .map_err(|e| anyhow::anyhow!(e))?
    } else {
        let kind_lc = kind.to_ascii_lowercase();
        let source_kind = match kind_lc.as_str() {
            "bundle" => unity_asset::environment::BinarySourceKind::AssetBundle,
            "serialized" => unity_asset::environment::BinarySourceKind::SerializedFile,
            other => anyhow::bail!("Unknown --kind: {} (expected: bundle|serialized)", other),
        };

        if source_kind == unity_asset::environment::BinarySourceKind::AssetBundle
            && asset_index.is_none()
        {
            anyhow::bail!("--asset-index is required when --kind bundle");
        }

        let path_id = path_id
            .ok_or_else(|| anyhow::anyhow!("--path-id is required unless --key is provided"))?;
        let source = source
            .ok_or_else(|| anyhow::anyhow!("--source is required unless --key is provided"))?;

        unity_asset::environment::BinaryObjectKey {
            source: BinarySource::path(&source),
            source_kind,
            asset_index,
            path_id,
        }
    };

    let resolved_source = resolve_loaded_source(&env, key.source_kind, &key.source)?;
    key.source = resolved_source.clone();

    let obj = env.read_binary_object_key(&key)?;

    println!(
        "Object: {} (class_id={}, byte_size={}, byte_start={}, byte_order={:?})",
        obj.describe(),
        obj.class_id(),
        obj.byte_size(),
        obj.byte_start(),
        obj.byte_order()
    );
    println!(
        "Source: {} (kind={:?}, asset_index={:?}, path_id={})",
        resolved_source, key.source_kind, key.asset_index, key.path_id
    );
    println!("Key: {}", key);

    let filter_lc = filter.to_ascii_lowercase();
    let mut printed = 0usize;

    let mut names: Vec<_> = obj.as_unity_class().properties().keys().collect();
    names.sort();
    println!("Properties: {}", names.len());

    for name in names {
        let Some(value) = obj.as_unity_class().get(name.as_str()) else {
            continue;
        };
        print_unity_value_tree(
            name,
            value,
            0,
            max_depth,
            max_items,
            max_array,
            &filter_lc,
            &mut printed,
        );
        if printed >= max_items {
            println!("... (truncated: max_items={})", max_items);
            break;
        }
    }

    Ok(())
}

fn resolve_loaded_source(
    env: &Environment,
    kind: unity_asset::environment::BinarySourceKind,
    requested: &BinarySource,
) -> Result<BinarySource> {
    let is_loaded = match kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => {
            env.bundles().contains_key(requested)
        }
        unity_asset::environment::BinarySourceKind::SerializedFile => {
            env.binary_assets().contains_key(requested)
        }
    };
    if is_loaded {
        return Ok(requested.clone());
    }

    let BinarySource::Path(requested_path) = requested else {
        anyhow::bail!("Source not found in loaded environment: {:?}", requested);
    };

    let keys: Vec<&PathBuf> = match kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => env
            .bundles()
            .keys()
            .filter_map(|k| match k {
                BinarySource::Path(p) => Some(p),
                _ => None,
            })
            .collect(),
        unity_asset::environment::BinarySourceKind::SerializedFile => env
            .binary_assets()
            .keys()
            .filter_map(|k| match k {
                BinarySource::Path(p) => Some(p),
                _ => None,
            })
            .collect(),
    };

    let requested_canon = std::fs::canonicalize(requested_path).ok();
    if let Some(requested_canon) = requested_canon {
        let mut matches = Vec::new();
        for k in &keys {
            if let Ok(k_canon) = std::fs::canonicalize(k) {
                if k_canon == requested_canon {
                    matches.push((*k).clone());
                }
            }
        }
        if matches.len() == 1 {
            return Ok(BinarySource::path(matches[0].clone()));
        }
        if matches.len() > 1 {
            anyhow::bail!(
                "Ambiguous source path: {:?} matches multiple loaded sources",
                requested_path
            );
        }
    }

    if let Some(file_name) = requested_path.file_name() {
        let mut matches: Vec<PathBuf> = keys
            .iter()
            .filter(|p| p.file_name() == Some(file_name))
            .map(|p| (*p).clone())
            .collect();
        matches.sort();
        matches.dedup();
        if matches.len() == 1 {
            return Ok(BinarySource::path(matches[0].clone()));
        }
    }

    let mut available: Vec<String> = keys.iter().map(|p| p.display().to_string()).collect();
    available.sort();

    anyhow::bail!(
        "Source not found in loaded environment: {:?} (kind={:?}). Loaded sources:\n- {}",
        requested_path,
        kind,
        available.join("\n- ")
    )
}

fn print_unity_value_tree(
    path: &str,
    value: &UnityValue,
    depth: usize,
    max_depth: usize,
    max_items: usize,
    max_array: usize,
    filter_lc: &str,
    printed: &mut usize,
) {
    if *printed >= max_items {
        return;
    }

    let path_lc = path.to_ascii_lowercase();
    if !filter_lc.is_empty() && !path_lc.contains(filter_lc) {
        // Still traverse children so that deep matches can be printed.
        match value {
            UnityValue::Array(arr) if depth < max_depth => {
                for (i, item) in arr.iter().take(max_array).enumerate() {
                    let child_path = format!("{}[{}]", path, i);
                    print_unity_value_tree(
                        &child_path,
                        item,
                        depth + 1,
                        max_depth,
                        max_items,
                        max_array,
                        filter_lc,
                        printed,
                    );
                    if *printed >= max_items {
                        break;
                    }
                }
            }
            UnityValue::Object(obj) if depth < max_depth => {
                for (k, v) in obj.iter() {
                    let child_path = format!("{}.{}", path, k);
                    print_unity_value_tree(
                        &child_path,
                        v,
                        depth + 1,
                        max_depth,
                        max_items,
                        max_array,
                        filter_lc,
                        printed,
                    );
                    if *printed >= max_items {
                        break;
                    }
                }
            }
            _ => {}
        }
        return;
    }

    let indent = "  ".repeat(depth);
    match value {
        UnityValue::Null => {
            println!("{}{}: Null", indent, path);
            *printed += 1;
        }
        UnityValue::Bool(b) => {
            println!("{}{}: Bool({})", indent, path, b);
            *printed += 1;
        }
        UnityValue::Integer(i) => {
            println!("{}{}: Integer({})", indent, path, i);
            *printed += 1;
        }
        UnityValue::Float(f) => {
            println!("{}{}: Float({})", indent, path, f);
            *printed += 1;
        }
        UnityValue::String(s) => {
            let preview = if s.chars().count() > 200 {
                let head: String = s.chars().take(200).collect();
                format!("{}...(len={})", head, s.len())
            } else {
                s.clone()
            };
            println!("{}{}: String({:?})", indent, path, preview);
            *printed += 1;
        }
        UnityValue::Array(arr) => {
            println!("{}{}: Array(len={})", indent, path, arr.len());
            *printed += 1;
            if depth >= max_depth {
                return;
            }
            for (i, item) in arr.iter().take(max_array).enumerate() {
                let child_path = format!("{}[{}]", path, i);
                print_unity_value_tree(
                    &child_path,
                    item,
                    depth + 1,
                    max_depth,
                    max_items,
                    max_array,
                    filter_lc,
                    printed,
                );
                if *printed >= max_items {
                    return;
                }
            }
            if arr.len() > max_array {
                println!(
                    "{}  {}: ... ({} more items)",
                    indent,
                    path,
                    arr.len().saturating_sub(max_array)
                );
                *printed += 1;
            }
        }
        UnityValue::Object(obj) => {
            println!("{}{}: Object(keys={})", indent, path, obj.len());
            *printed += 1;
            if depth >= max_depth {
                return;
            }
            for (k, v) in obj.iter() {
                let child_path = format!("{}.{}", path, k);
                print_unity_value_tree(
                    &child_path,
                    v,
                    depth + 1,
                    max_depth,
                    max_items,
                    max_array,
                    filter_lc,
                    printed,
                );
                if *printed >= max_items {
                    return;
                }
            }
        }
    }
}
