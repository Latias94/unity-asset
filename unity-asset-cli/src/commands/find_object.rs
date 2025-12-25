use crate::fast_path;
use crate::shared::{
    AppContext, build_environment, class_name_for_id, cli_warn, load_environment_input,
    load_typetree_registry,
};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use unity_asset::UnityValue;
use unity_asset::environment::{BinaryObjectKey, BinarySource, Environment};
use unity_asset_binary::bundle::AssetBundle;
use unity_asset_binary::object::UnityObject;
use unity_asset_binary::typetree::{TypeTreeParseMode, TypeTreeParseOptions, TypeTreeRegistry};

pub(crate) fn run(
    input: PathBuf,
    pattern: String,
    name: String,
    class_id: Vec<i32>,
    class_name: String,
    limit: Option<usize>,
    include_unresolved: bool,
    verbose: bool,
    ctx: &AppContext,
) -> Result<()> {
    if let Ok(true) = find_object_fast(
        &input,
        &pattern,
        &name,
        &class_id,
        &class_name,
        limit,
        include_unresolved,
        verbose,
        ctx.strict,
        ctx.show_warnings,
        ctx.typetree_registries(),
    ) {
        return Ok(());
    }

    find_object_env_fallback(
        input,
        pattern,
        name,
        class_id,
        class_name,
        limit,
        include_unresolved,
        verbose,
        ctx.strict,
        ctx.show_warnings,
        ctx.typetree_registries(),
    )
}

#[allow(clippy::too_many_arguments)]
fn find_object_env_fallback(
    input: PathBuf,
    pattern: String,
    name: String,
    class_id: Vec<i32>,
    class_name: String,
    limit: Option<usize>,
    include_unresolved: bool,
    verbose: bool,
    strict: bool,
    show_warnings: bool,
    typetree_registries: &[PathBuf],
) -> Result<()> {
    let mut env = build_environment(strict, show_warnings, typetree_registries)?;
    load_environment_input(&mut env, &input)?;

    let pattern_lc = pattern.to_ascii_lowercase();
    let name_lc = name.to_ascii_lowercase();
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
                        let name = class_name_for_id(type_id);
                        if !name.as_ref().to_ascii_lowercase().contains(&class_name_lc) {
                            continue;
                        }
                    }
                    if !name_lc.is_empty() {
                        let matches = match env.peek_binary_object_name(key) {
                            Ok(Some(found)) => found.to_ascii_lowercase().contains(&name_lc),
                            Ok(None) => false,
                            Err(e) => {
                                cli_warn(
                                    show_warnings,
                                    format!("peek_name failed for key={}: {}", key, e),
                                );
                                false
                            }
                        };
                        if !matches {
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
                    let name = class_name_for_id(type_id);
                    if !name.as_ref().to_ascii_lowercase().contains(&class_name_lc) {
                        continue;
                    }
                }
                if !name_lc.is_empty() {
                    let matches = match env.peek_binary_object_name(key) {
                        Ok(Some(found)) => found.to_ascii_lowercase().contains(&name_lc),
                        Ok(None) => false,
                        Err(e) => {
                            cli_warn(
                                show_warnings,
                                format!("peek_name failed for key={}: {}", key, e),
                            );
                            false
                        }
                    };
                    if !matches {
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

#[allow(clippy::too_many_arguments)]
fn find_object_fast(
    input: &PathBuf,
    pattern: &str,
    name: &str,
    class_id: &[i32],
    class_name: &str,
    limit: Option<usize>,
    include_unresolved: bool,
    verbose: bool,
    strict: bool,
    show_warnings: bool,
    typetree_registries: &[PathBuf],
) -> Result<bool> {
    let registry = load_typetree_registry(typetree_registries)?;
    let typetree_options = if strict {
        TypeTreeParseOptions {
            mode: TypeTreeParseMode::Strict,
        }
    } else {
        TypeTreeParseOptions {
            mode: TypeTreeParseMode::Lenient,
        }
    };

    let candidate_paths = fast_path::collect_candidate_paths(input)?;

    let pattern_lc = pattern.to_ascii_lowercase();
    let name_lc = name.to_ascii_lowercase();
    let class_name_lc = class_name.to_ascii_lowercase();

    let mut processed_any_bundle = false;
    let mut count = 0usize;

    for path in candidate_paths {
        if let Some(max) = limit {
            if count >= max {
                break;
            }
        }

        if !fast_path::is_unityfs_bundle_path(&path) {
            continue;
        }

        let options = fast_path::bundle_list_options();
        let mut bundle = match fast_path::load_bundle_for_list(&path, options) {
            Ok(v) => v,
            Err(_) => continue,
        };
        processed_any_bundle = true;

        let bundle_source = BinarySource::path(&path);
        let asset_nodes = fast_path::bundle_asset_nodes(&bundle);
        let asset_names: Vec<String> = asset_nodes.iter().map(|n| n.name.clone()).collect();

        let entries = extract_bundle_container_entries_fast(
            &mut bundle,
            &bundle_source,
            &asset_nodes,
            &asset_names,
            registry.as_ref(),
            typetree_options,
            show_warnings,
        );

        let mut entries = match entries {
            Ok(v) => v,
            Err(e) => {
                if show_warnings {
                    eprintln!(
                        "warning: failed to extract m_Container for {:?}: {}",
                        path, e
                    );
                }
                continue;
            }
        };
        entries.sort_by(|a, b| a.asset_path.cmp(&b.asset_path));

        let need_cache =
            verbose || !class_id.is_empty() || !class_name_lc.is_empty() || !name_lc.is_empty();
        let mut file_cache: Vec<Option<unity_asset_binary::asset::SerializedFile>> = if need_cache {
            std::iter::repeat_with(|| None)
                .take(asset_nodes.len())
                .collect()
        } else {
            Vec::new()
        };

        for entry in entries {
            if let Some(max) = limit {
                if count >= max {
                    return Ok(true);
                }
            }

            if !pattern_lc.is_empty()
                && !entry.asset_path.to_ascii_lowercase().contains(&pattern_lc)
            {
                continue;
            }

            if entry.key.is_none()
                && (!include_unresolved || !class_id.is_empty() || !class_name_lc.is_empty())
            {
                continue;
            }

            if verbose {
                if let Some(key) = &entry.key {
                    let (type_id, byte_size) = lookup_object_type_info_fast(
                        &bundle,
                        &asset_nodes,
                        &mut file_cache,
                        key,
                        registry.as_ref(),
                    );

                    if !class_id.is_empty() && !class_id.contains(&type_id) {
                        continue;
                    }
                    if !class_name_lc.is_empty() {
                        let name = class_name_for_id(type_id);
                        if !name.as_ref().to_ascii_lowercase().contains(&class_name_lc) {
                            continue;
                        }
                    }
                    if !name_lc.is_empty() {
                        let matches = match peek_object_name_fast(
                            &bundle,
                            &asset_nodes,
                            &mut file_cache,
                            key,
                            registry.as_ref(),
                            typetree_options,
                        ) {
                            Ok(Some(found)) => found.to_ascii_lowercase().contains(&name_lc),
                            Ok(None) => false,
                            Err(e) => {
                                cli_warn(
                                    show_warnings,
                                    format!("peek_name failed for key={}: {}", key, e),
                                );
                                false
                            }
                        };
                        if !matches {
                            continue;
                        }
                    }
                    println!(
                        "{} -> key={} (class_id={}, byte_size={})",
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
                let (type_id, _byte_size) = if class_id.is_empty() && class_name_lc.is_empty() {
                    (0, 0)
                } else {
                    lookup_object_type_info_fast(
                        &bundle,
                        &asset_nodes,
                        &mut file_cache,
                        key,
                        registry.as_ref(),
                    )
                };

                if !class_id.is_empty() && !class_id.contains(&type_id) {
                    continue;
                }
                if !class_name_lc.is_empty() {
                    let name = class_name_for_id(type_id);
                    if !name.as_ref().to_ascii_lowercase().contains(&class_name_lc) {
                        continue;
                    }
                }
                if !name_lc.is_empty() {
                    let matches = match peek_object_name_fast(
                        &bundle,
                        &asset_nodes,
                        &mut file_cache,
                        key,
                        registry.as_ref(),
                        typetree_options,
                    ) {
                        Ok(Some(found)) => found.to_ascii_lowercase().contains(&name_lc),
                        Ok(None) => false,
                        Err(e) => {
                            cli_warn(
                                show_warnings,
                                format!("peek_name failed for key={}: {}", key, e),
                            );
                            false
                        }
                    };
                    if !matches {
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

    Ok(processed_any_bundle)
}

fn extract_bundle_container_entries_fast(
    bundle: &mut AssetBundle,
    bundle_source: &BinarySource,
    asset_nodes: &[unity_asset_binary::bundle::DirectoryNode],
    asset_names: &[String],
    registry: Option<&Arc<dyn TypeTreeRegistry>>,
    typetree_options: TypeTreeParseOptions,
    show_warnings: bool,
) -> Result<Vec<unity_asset::environment::BundleContainerEntry>> {
    for (asset_index, node) in asset_nodes.iter().enumerate() {
        let bytes = bundle
            .extract_node_data(node)
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut file = unity_asset_binary::asset::SerializedFileParser::from_bytes(bytes)
            .map_err(|e| anyhow::anyhow!(e))?;
        if let Some(registry) = registry.cloned() {
            file.set_type_tree_registry(Some(registry));
        }

        let mut out: Vec<unity_asset::environment::BundleContainerEntry> = Vec::new();
        for object in file.object_handles() {
            if object.class_id() != 142 {
                continue;
            }

            if file.enable_type_tree {
                match object.read_with_options(typetree_options) {
                    Ok(obj) => {
                        if show_warnings {
                            for w in obj.typetree_warnings() {
                                eprintln!(
                                    "warning: typetree key={} field={} error={}",
                                    BinaryObjectKey {
                                        source: bundle_source.clone(),
                                        source_kind:
                                            unity_asset::environment::BinarySourceKind::AssetBundle,
                                        asset_index: Some(asset_index),
                                        path_id: object.path_id(),
                                    },
                                    w.field,
                                    w.error
                                );
                            }
                        }
                        out.extend(extract_container_entries_from_typetree(
                            bundle_source,
                            asset_index,
                            &file,
                            asset_names,
                            &obj,
                        ));
                        if !out.is_empty() {
                            return Ok(out);
                        }
                    }
                    Err(e) => {
                        if show_warnings {
                            eprintln!(
                                "warning: typetree container parse failed (bundle={}, asset_index={}, path_id={}): {}",
                                bundle_source,
                                asset_index,
                                object.path_id(),
                                e
                            );
                        }
                    }
                }
            }

            if let Ok(raw_entries) = file.assetbundle_container_raw(object.info()) {
                for (asset_path, file_id, path_id) in raw_entries {
                    if path_id == 0 {
                        continue;
                    }
                    let key = resolve_pptr_in_bundle(
                        bundle_source,
                        asset_index,
                        &file,
                        asset_names,
                        file_id,
                        path_id,
                    );
                    out.push(unity_asset::environment::BundleContainerEntry {
                        bundle_source: bundle_source.clone(),
                        asset_index,
                        asset_path,
                        file_id,
                        path_id,
                        key,
                    });
                }
                if !out.is_empty() {
                    return Ok(out);
                }
            }
        }
    }

    Ok(Vec::new())
}

fn extract_container_entries_from_typetree(
    bundle_source: &BinarySource,
    context_asset_index: usize,
    context_file: &unity_asset_binary::asset::SerializedFile,
    asset_names: &[String],
    parsed: &UnityObject,
) -> Vec<unity_asset::environment::BundleContainerEntry> {
    let mut out = Vec::new();
    let Some(UnityValue::Array(items)) = parsed.class.get("m_Container") else {
        return out;
    };

    for item in items {
        let (asset_path, second) = match item {
            UnityValue::Array(pair) if pair.len() == 2 => {
                let Some(asset_path) = pair[0].as_str() else {
                    continue;
                };
                (asset_path.to_string(), &pair[1])
            }
            UnityValue::Object(obj) => {
                let first = obj.get("first").and_then(|v| v.as_str());
                let second = obj.get("second").or_else(|| obj.get("value"));
                let (Some(first), Some(second)) = (first, second) else {
                    continue;
                };
                (first.to_string(), second)
            }
            _ => continue,
        };

        let Some((file_id, path_id)) = scan_pptr_value(second) else {
            continue;
        };
        if path_id == 0 {
            continue;
        }

        let key = resolve_pptr_in_bundle(
            bundle_source,
            context_asset_index,
            context_file,
            asset_names,
            file_id,
            path_id,
        );

        out.push(unity_asset::environment::BundleContainerEntry {
            bundle_source: bundle_source.clone(),
            asset_index: context_asset_index,
            asset_path,
            file_id,
            path_id,
            key,
        });
    }

    out
}

fn scan_pptr_value(value: &UnityValue) -> Option<(i32, i64)> {
    match value {
        UnityValue::Object(obj) => {
            let file_id = obj
                .get("fileID")
                .or_else(|| obj.get("m_FileID"))
                .and_then(|v| v.as_i64())
                .and_then(|v| i32::try_from(v).ok());
            let path_id = obj
                .get("pathID")
                .or_else(|| obj.get("m_PathID"))
                .and_then(|v| v.as_i64());

            if let (Some(file_id), Some(path_id)) = (file_id, path_id) {
                return Some((file_id, path_id));
            }

            for (_, v) in obj.iter() {
                if let Some(pptr) = scan_pptr_value(v) {
                    return Some(pptr);
                }
            }

            None
        }
        UnityValue::Array(items) => items.iter().find_map(scan_pptr_value),
        _ => None,
    }
}

fn resolve_pptr_in_bundle(
    bundle_source: &BinarySource,
    context_asset_index: usize,
    context_file: &unity_asset_binary::asset::SerializedFile,
    asset_names: &[String],
    file_id: i32,
    path_id: i64,
) -> Option<BinaryObjectKey> {
    if file_id == 0 {
        return Some(BinaryObjectKey {
            source: bundle_source.clone(),
            source_kind: unity_asset::environment::BinarySourceKind::AssetBundle,
            asset_index: Some(context_asset_index),
            path_id,
        });
    }
    if file_id < 0 {
        return None;
    }

    let idx: usize = (file_id - 1).try_into().ok()?;
    let external = context_file.externals.get(idx)?;
    let external_norm = external.path.replace('\\', "/");
    let external_file_name = std::path::Path::new(&external_norm)
        .file_name()
        .and_then(|n| n.to_str());

    let mut candidates: Vec<(usize, &String)> = asset_names.iter().enumerate().collect();
    candidates.sort_by(|a, b| a.1.cmp(b.1));

    let (asset_index, _) = candidates.into_iter().find(|(_, name)| {
        let name_norm = name.replace('\\', "/");
        if name_norm == external_norm {
            return true;
        }
        if name_norm.ends_with(&external_norm) || external_norm.ends_with(&name_norm) {
            return true;
        }
        match external_file_name {
            Some(file_name) => {
                std::path::Path::new(&name_norm)
                    .file_name()
                    .and_then(|n| n.to_str())
                    == Some(file_name)
            }
            None => false,
        }
    })?;

    Some(BinaryObjectKey {
        source: bundle_source.clone(),
        source_kind: unity_asset::environment::BinarySourceKind::AssetBundle,
        asset_index: Some(asset_index),
        path_id,
    })
}

fn lookup_object_type_info_fast(
    bundle: &AssetBundle,
    asset_nodes: &[unity_asset_binary::bundle::DirectoryNode],
    cache: &mut [Option<unity_asset_binary::asset::SerializedFile>],
    key: &BinaryObjectKey,
    registry: Option<&Arc<dyn TypeTreeRegistry>>,
) -> (i32, u32) {
    if key.source_kind != unity_asset::environment::BinarySourceKind::AssetBundle {
        return (0, 0);
    }
    let Some(asset_index) = key.asset_index else {
        return (0, 0);
    };
    if asset_index >= asset_nodes.len() || asset_index >= cache.len() {
        return (0, 0);
    }

    if cache[asset_index].is_none() {
        let node = &asset_nodes[asset_index];
        let bytes = match bundle.extract_node_data(node) {
            Ok(v) => v,
            Err(_) => return (0, 0),
        };
        if let Ok(mut file) = unity_asset_binary::asset::SerializedFileParser::from_bytes(bytes) {
            if let Some(registry) = registry.cloned() {
                file.set_type_tree_registry(Some(registry));
            }
            cache[asset_index] = Some(file);
        }
    }

    cache[asset_index]
        .as_ref()
        .and_then(|f| f.find_object(key.path_id))
        .map(|info| (info.type_id, info.byte_size))
        .unwrap_or((0, 0))
}

fn peek_object_name_fast(
    bundle: &AssetBundle,
    asset_nodes: &[unity_asset_binary::bundle::DirectoryNode],
    cache: &mut [Option<unity_asset_binary::asset::SerializedFile>],
    key: &BinaryObjectKey,
    registry: Option<&Arc<dyn TypeTreeRegistry>>,
    options: TypeTreeParseOptions,
) -> Result<Option<String>> {
    if key.source_kind != unity_asset::environment::BinarySourceKind::AssetBundle {
        return Ok(None);
    }
    let Some(asset_index) = key.asset_index else {
        return Ok(None);
    };
    if asset_index >= asset_nodes.len() || asset_index >= cache.len() {
        return Ok(None);
    }

    if cache[asset_index].is_none() {
        let node = &asset_nodes[asset_index];
        let bytes = bundle
            .extract_node_data(node)
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut file = unity_asset_binary::asset::SerializedFileParser::from_bytes(bytes)
            .map_err(|e| anyhow::anyhow!(e))?;
        if let Some(registry) = registry.cloned() {
            file.set_type_tree_registry(Some(registry));
        }
        cache[asset_index] = Some(file);
    }

    let file = cache[asset_index].as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "failed to parse serialized file for asset_index={}",
            asset_index
        )
    })?;
    let handle = file.find_object_handle(key.path_id).ok_or_else(|| {
        anyhow::anyhow!(
            "object not found: path_id={} (asset_index={})",
            key.path_id,
            asset_index
        )
    })?;
    Ok(handle
        .peek_name_with_options(options)
        .map_err(|e| anyhow::anyhow!(e))?)
}

fn lookup_object_type_info(env: &Environment, key: &BinaryObjectKey) -> (i32, u32) {
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
