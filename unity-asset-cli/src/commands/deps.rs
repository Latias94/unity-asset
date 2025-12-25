use crate::fast_path;
use crate::shared::{
    AppContext, build_environment, cli_warn, load_environment_input, load_serialized_file_for_scan,
    load_typetree_registry, resolve_loaded_source,
};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use unity_asset::environment::BinarySource;
use unity_asset_binary::bundle::BundleLoadOptions;

#[derive(Debug, Serialize)]
struct DepsOutput {
    source: String,
    source_kind: String,
    asset_index: Option<usize>,
    unity_version: String,
    object_count: usize,
    deps: unity_asset_binary::metadata::DependencyInfo,
}

fn deps_analyze_and_print(
    resolved_source: &BinarySource,
    source_kind: unity_asset::environment::BinarySourceKind,
    asset_index: Option<usize>,
    file: &unity_asset_binary::asset::SerializedFile,
    format: &str,
    names: bool,
    max_edges: usize,
) -> Result<()> {
    use unity_asset_binary::metadata::DependencyAnalyzer;

    let objects: Vec<&unity_asset_binary::asset::ObjectInfo> = file.objects.iter().collect();
    let mut analyzer = DependencyAnalyzer::new();
    let deps = analyzer.analyze_dependencies_in_asset(file, &objects)?;

    let fmt = format.to_ascii_lowercase();
    match fmt.as_str() {
        "summary" => {
            println!(
                "Source: {} (kind={:?}, asset_index={:?})",
                resolved_source, source_kind, asset_index
            );
            println!("Unity: {}", file.unity_version);
            println!("Objects: {}", file.objects.len());
            println!(
                "Internal refs: {} (edges={})",
                deps.internal_references.len(),
                deps.dependency_graph.edges.len()
            );
            println!("External refs: {}", deps.external_references.len());
            println!("Roots: {}", deps.dependency_graph.root_objects.len());
            println!("Leaves: {}", deps.dependency_graph.leaf_objects.len());
            println!("Cycles: {}", deps.circular_dependencies.len());
        }
        "json" => {
            let out = DepsOutput {
                source: resolved_source.to_string(),
                source_kind: match source_kind {
                    unity_asset::environment::BinarySourceKind::AssetBundle => "bundle",
                    unity_asset::environment::BinarySourceKind::SerializedFile => "serialized",
                }
                .to_string(),
                asset_index,
                unity_version: file.unity_version.clone(),
                object_count: file.objects.len(),
                deps,
            };
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        "edges" => {
            let mut printed = 0usize;
            let mut name_cache: std::collections::HashMap<i64, String> =
                std::collections::HashMap::new();

            for (from, to) in deps.dependency_graph.edges.iter().take(max_edges) {
                if printed >= max_edges {
                    break;
                }
                if names {
                    let from_name = name_cache.get(from).cloned().unwrap_or_else(|| {
                        let n = file
                            .find_object_handle(*from)
                            .and_then(|h| h.peek_name().ok().flatten())
                            .unwrap_or_default();
                        name_cache.insert(*from, n.clone());
                        n
                    });
                    let to_name = name_cache.get(to).cloned().unwrap_or_else(|| {
                        let n = file
                            .find_object_handle(*to)
                            .and_then(|h| h.peek_name().ok().flatten())
                            .unwrap_or_default();
                        name_cache.insert(*to, n.clone());
                        n
                    });
                    println!("{}({}) -> {}({})", from, from_name, to, to_name);
                } else {
                    println!("{} -> {}", from, to);
                }
                printed += 1;
            }
            if deps.dependency_graph.edges.len() > max_edges {
                println!(
                    "... (truncated: edges={}, max_edges={})",
                    deps.dependency_graph.edges.len(),
                    max_edges
                );
            }
        }
        "dot" => {
            println!("digraph deps {{");
            for (from, to) in deps.dependency_graph.edges.iter().take(max_edges) {
                println!("  \"{}\" -> \"{}\";", from, to);
            }
            if deps.dependency_graph.edges.len() > max_edges {
                println!(
                    "  // truncated: edges={}, max_edges={}",
                    deps.dependency_graph.edges.len(),
                    max_edges
                );
            }
            println!("}}");
        }
        other => anyhow::bail!(
            "Invalid --format: {} (expected summary|edges|dot|json)",
            other
        ),
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn deps_fast(
    input: &PathBuf,
    kind: &str,
    source: Option<&PathBuf>,
    asset_index: Option<usize>,
    format: &str,
    names: bool,
    max_edges: usize,
    show_warnings: bool,
    typetree_registries: &[PathBuf],
) -> Result<bool> {
    let registry = load_typetree_registry(typetree_registries)?;

    let kind_lc = kind.to_ascii_lowercase();
    let source_kind = match kind_lc.as_str() {
        "bundle" => unity_asset::environment::BinarySourceKind::AssetBundle,
        "serialized" => unity_asset::environment::BinarySourceKind::SerializedFile,
        _ => return Ok(false),
    };

    let candidate_paths = fast_path::collect_candidate_paths(input)?;

    let requested_source = source.map(|v| v.to_path_buf());

    let mut matching: Vec<PathBuf> = Vec::new();

    match source_kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => {
            for path in &candidate_paths {
                if !fast_path::is_unityfs_bundle_path(path) {
                    continue;
                }
                if let Some(req) = requested_source.as_ref() {
                    if !fast_path::path_matches_requested(path, req) {
                        continue;
                    }
                }
                matching.push(path.clone());
            }
        }
        unity_asset::environment::BinarySourceKind::SerializedFile => {
            for path in &candidate_paths {
                if !fast_path::is_serialized_file_path(path) {
                    continue;
                }
                if let Some(req) = requested_source.as_ref() {
                    if !fast_path::path_matches_requested(path, req) {
                        continue;
                    }
                }
                matching.push(path.clone());
            }
        }
    }

    let path = match matching.as_slice() {
        [] => return Ok(false),
        [only] => only.clone(),
        _ => return Ok(false),
    };

    match source_kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => {
            let options = BundleLoadOptions::lazy();
            let bundle = match fast_path::load_bundle_for_list(&path, options) {
                Ok(v) => v,
                Err(e) => {
                    cli_warn(
                        show_warnings,
                        format!("failed to parse bundle {:?}: {}", path, e),
                    );
                    return Ok(false);
                }
            };

            let idx = match asset_index {
                Some(v) => v,
                None => return Ok(false),
            };

            let asset_nodes = fast_path::bundle_asset_nodes(&bundle);
            if idx >= asset_nodes.len() {
                return Ok(false);
            }

            let source_key = BinarySource::path(&path);
            let node = &asset_nodes[idx];
            let bytes = match bundle.extract_node_data(node) {
                Ok(v) => v,
                Err(e) => {
                    cli_warn(
                        show_warnings,
                        format!("failed to extract bundle node for deps: {}", e),
                    );
                    return Ok(false);
                }
            };
            let mut file = unity_asset_binary::asset::SerializedFileParser::from_bytes(bytes)?;
            if let Some(registry) = registry {
                file.set_type_tree_registry(Some(registry));
            }

            deps_analyze_and_print(
                &source_key,
                unity_asset::environment::BinarySourceKind::AssetBundle,
                Some(idx),
                &file,
                format,
                names,
                max_edges,
            )?;
        }
        unity_asset::environment::BinarySourceKind::SerializedFile => {
            let mut file = match load_serialized_file_for_scan(&path) {
                Ok(v) => v,
                Err(e) => {
                    cli_warn(
                        show_warnings,
                        format!("failed to parse SerializedFile {:?}: {}", path, e),
                    );
                    return Ok(false);
                }
            };
            if let Some(registry) = registry {
                file.set_type_tree_registry(Some(registry));
            }

            let source_key = BinarySource::path(&path);
            deps_analyze_and_print(
                &source_key,
                unity_asset::environment::BinarySourceKind::SerializedFile,
                None,
                &file,
                format,
                names,
                max_edges,
            )?;
        }
    }

    Ok(true)
}

pub(crate) fn run(
    input: PathBuf,
    kind: String,
    source: Option<PathBuf>,
    asset_index: Option<usize>,
    format: String,
    names: bool,
    max_edges: usize,
    ctx: &AppContext,
) -> Result<()> {
    let kind_lc = kind.to_ascii_lowercase();
    if kind_lc == "bundle" && asset_index.is_none() {
        anyhow::bail!("--asset-index is required when --kind bundle");
    }
    if kind_lc == "serialized" && asset_index.is_some() {
        anyhow::bail!("--asset-index only applies to --kind bundle");
    }

    if let Ok(true) = deps_fast(
        &input,
        &kind,
        source.as_ref(),
        asset_index,
        &format,
        names,
        max_edges,
        ctx.show_warnings,
        ctx.typetree_registries(),
    ) {
        return Ok(());
    }

    let mut env = build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;
    load_environment_input(&mut env, &input)?;

    let source_kind = match kind_lc.as_str() {
        "bundle" => unity_asset::environment::BinarySourceKind::AssetBundle,
        "serialized" => unity_asset::environment::BinarySourceKind::SerializedFile,
        other => anyhow::bail!("Invalid --kind: {} (expected bundle|serialized)", other),
    };

    let (resolved_source, resolved_asset_index, file) = match source_kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => {
            let idx = asset_index
                .ok_or_else(|| anyhow::anyhow!("--asset-index is required when --kind bundle"))?;
            let bundle_source = if let Some(src) = source {
                let req = BinarySource::path(&src);
                resolve_loaded_source(&env, source_kind, &req)?
            } else if env.bundles().len() == 1 {
                env.bundles()
                    .keys()
                    .next()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("No bundles loaded"))?
            } else {
                let mut available: Vec<String> = env
                    .bundles()
                    .keys()
                    .filter_map(|k| match k {
                        BinarySource::Path(p) => Some(p),
                        _ => None,
                    })
                    .map(|p| p.display().to_string())
                    .collect();
                available.sort();
                anyhow::bail!(
                    "--source is required when multiple bundles are loaded. Loaded bundles:\n- {}",
                    available.join("\n- ")
                );
            };

            let bundle = env
                .bundles()
                .get(&bundle_source)
                .ok_or_else(|| anyhow::anyhow!("Bundle not found: {}", bundle_source))?;
            let file = bundle
                .assets
                .get(idx)
                .ok_or_else(|| anyhow::anyhow!("Bundle asset_index out of range: {}", idx))?;
            (bundle_source, Some(idx), file)
        }
        unity_asset::environment::BinarySourceKind::SerializedFile => {
            let asset_source = if let Some(src) = source {
                let req = BinarySource::path(&src);
                resolve_loaded_source(&env, source_kind, &req)?
            } else if env.binary_assets().len() == 1 {
                env.binary_assets()
                    .keys()
                    .next()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("No serialized files loaded"))?
            } else {
                let mut available: Vec<String> = env
                    .binary_assets()
                    .keys()
                    .filter_map(|k| match k {
                        BinarySource::Path(p) => Some(p),
                        _ => None,
                    })
                    .map(|p| p.display().to_string())
                    .collect();
                available.sort();
                anyhow::bail!(
                    "--source is required when multiple serialized files are loaded. Loaded serialized files:\n- {}",
                    available.join("\n- ")
                );
            };

            let file = env
                .binary_assets()
                .get(&asset_source)
                .ok_or_else(|| anyhow::anyhow!("SerializedFile not found: {}", asset_source))?;
            (asset_source, None, file)
        }
    };

    deps_analyze_and_print(
        &resolved_source,
        source_kind,
        resolved_asset_index,
        file,
        &format,
        names,
        max_edges,
    )
}
