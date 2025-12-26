use crate::fast_path;
use crate::shared::{
    AppContext, build_environment, cli_warn, load_environment_input, load_serialized_file_for_scan,
    load_typetree_registry, resolve_loaded_source,
};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use unity_asset::environment::{BinaryObjectKey, BinarySource};
use unity_asset_binary::bundle::BundleLoadOptions;
use unity_asset_binary::shared_bytes::SharedBytes;

#[derive(Debug, Serialize)]
struct ScanPPtrRecord {
    key: String,
    source: String,
    source_kind: String,
    asset_index: Option<usize>,
    path_id: i64,
    type_id: i32,
    byte_size: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    internal: Vec<i64>,
    external: Vec<ScanPPtrExternal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    typetree: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ScanPPtrExternal {
    file_id: i32,
    path_id: i64,
}

#[allow(clippy::too_many_arguments)]
fn scan_pptr_scan_file(
    source_key: &BinarySource,
    source_kind: unity_asset::environment::BinarySourceKind,
    asset_index_key: Option<usize>,
    file: &unity_asset_binary::asset::SerializedFile,
    class_id: &[i32],
    has_name_filter: bool,
    name_lc: &str,
    include_no_typetree: bool,
    json: bool,
    remaining: &mut usize,
) -> Result<()> {
    if *remaining == 0 {
        return Ok(());
    }

    for handle in file.object_handles() {
        if *remaining == 0 {
            break;
        }
        if !class_id.is_empty() && !class_id.contains(&handle.class_id()) {
            continue;
        }

        let obj_name = if has_name_filter {
            handle.peek_name().unwrap_or_default()
        } else {
            None
        };
        if has_name_filter {
            let Some(n) = obj_name.as_ref() else {
                continue;
            };
            if !n.to_ascii_lowercase().contains(name_lc) {
                continue;
            }
        }

        let key = BinaryObjectKey {
            source: source_key.clone(),
            source_kind,
            asset_index: asset_index_key,
            path_id: handle.path_id(),
        };

        let info = handle.info();
        let scan = handle.scan_pptrs()?;

        let (typetree_ok, mut internal, mut external) = match scan {
            Some(v) => (true, v.internal, v.external),
            None => (false, Vec::new(), Vec::new()),
        };
        if !typetree_ok && !include_no_typetree {
            continue;
        }

        internal.sort_unstable();
        internal.dedup();
        external.sort_unstable();
        external.dedup();

        let record = ScanPPtrRecord {
            key: key.to_string(),
            source: source_key.to_string(),
            source_kind: match source_kind {
                unity_asset::environment::BinarySourceKind::AssetBundle => "bundle",
                unity_asset::environment::BinarySourceKind::SerializedFile => "serialized",
            }
            .to_string(),
            asset_index: asset_index_key,
            path_id: handle.path_id(),
            type_id: handle.class_id(),
            byte_size: info.byte_size,
            name: obj_name,
            internal,
            external: external
                .into_iter()
                .map(|(file_id, path_id)| ScanPPtrExternal { file_id, path_id })
                .collect(),
            typetree: if include_no_typetree {
                Some(typetree_ok)
            } else {
                None
            },
        };

        if json {
            println!("{}", serde_json::to_string(&record)?);
        } else {
            println!(
                "key={} type_id={} byte_size={} internal={} external={}",
                record.key,
                record.type_id,
                record.byte_size,
                record.internal.len(),
                record.external.len()
            );
        }

        *remaining = remaining.saturating_sub(1);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_pptr_fast(
    input: &std::path::Path,
    kind: &str,
    source: Option<&PathBuf>,
    asset_index: Option<usize>,
    class_id: &[i32],
    name: &str,
    limit: Option<usize>,
    include_no_typetree: bool,
    json: bool,
    show_warnings: bool,
    typetree_registries: &[PathBuf],
) -> Result<bool> {
    let registry = load_typetree_registry(typetree_registries)?;

    let kind_lc = kind.to_ascii_lowercase();
    let scan_bundles = kind_lc == "all" || kind_lc == "bundle";
    let scan_serialized = kind_lc == "all" || kind_lc == "serialized";
    if !scan_bundles && !scan_serialized {
        anyhow::bail!("Invalid --kind: {} (expected all|bundle|serialized)", kind);
    }

    let name_lc = name.to_ascii_lowercase();
    let has_name_filter = !name_lc.is_empty();
    let mut remaining = limit.unwrap_or(usize::MAX);

    let requested_source = source.map(|v| v.to_path_buf());
    let candidate_paths = fast_path::collect_candidate_paths(input)?;

    let mut processed_any = false;

    if scan_bundles {
        for path in &candidate_paths {
            if remaining == 0 {
                break;
            }
            if let Some(req) = requested_source.as_ref() {
                if !fast_path::path_matches_requested(path, req) {
                    continue;
                }
            }

            if !fast_path::is_unityfs_bundle_path(path) {
                continue;
            }

            let options = BundleLoadOptions::lazy();
            let bundle = match fast_path::load_bundle_for_list(path, options) {
                Ok(v) => v,
                Err(e) => {
                    cli_warn(
                        show_warnings,
                        format!("failed to parse bundle {:?}: {}", path, e),
                    );
                    continue;
                }
            };

            let asset_nodes = fast_path::bundle_asset_nodes(&bundle);
            if let Some(idx) = asset_index {
                if idx >= asset_nodes.len() {
                    return Ok(false);
                }
            }

            let source_key = BinarySource::path(path);
            processed_any = true;

            if let Some(filter_idx) = asset_index {
                let Some(node) = asset_nodes.get(filter_idx) else {
                    return Ok(false);
                };
                let bytes = match bundle.extract_node_data(node) {
                    Ok(v) => v,
                    Err(e) => {
                        cli_warn(
                            show_warnings,
                            format!(
                                "failed to extract bundle node {:?} for scan-pptr: {}",
                                path, e
                            ),
                        );
                        continue;
                    }
                };
                let mut file =
                    match unity_asset_binary::asset::SerializedFileParser::from_bytes(bytes) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                if let Some(registry) = registry.clone() {
                    file.set_type_tree_registry(Some(registry));
                }

                scan_pptr_scan_file(
                    &source_key,
                    unity_asset::environment::BinarySourceKind::AssetBundle,
                    Some(filter_idx),
                    &file,
                    class_id,
                    has_name_filter,
                    &name_lc,
                    include_no_typetree,
                    json,
                    &mut remaining,
                )?;
                continue;
            }

            let shared = match bundle.data_arc() {
                Ok(v) => SharedBytes::from_arc(v),
                Err(e) => {
                    cli_warn(
                        show_warnings,
                        format!(
                            "failed to decompress bundle {:?} for scan-pptr: {}",
                            path, e
                        ),
                    );
                    continue;
                }
            };

            for (idx, node) in asset_nodes.iter().enumerate() {
                if remaining == 0 {
                    break;
                }

                let (start, end) = match fast_path::node_range(node) {
                    Ok(v) => v,
                    Err(e) => {
                        cli_warn(
                            show_warnings,
                            format!("invalid bundle node range ({}): {}", node.name, e),
                        );
                        continue;
                    }
                };

                let mut file =
                    match unity_asset_binary::asset::SerializedFileParser::from_shared_range(
                        shared.clone(),
                        start..end,
                    ) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                if let Some(registry) = registry.clone() {
                    file.set_type_tree_registry(Some(registry));
                }

                scan_pptr_scan_file(
                    &source_key,
                    unity_asset::environment::BinarySourceKind::AssetBundle,
                    Some(idx),
                    &file,
                    class_id,
                    has_name_filter,
                    &name_lc,
                    include_no_typetree,
                    json,
                    &mut remaining,
                )?;
            }
        }
    }

    if scan_serialized {
        for path in &candidate_paths {
            if remaining == 0 {
                break;
            }
            if let Some(req) = requested_source.as_ref() {
                if !fast_path::path_matches_requested(path, req) {
                    continue;
                }
            }

            if !fast_path::is_serialized_file_path(path) {
                continue;
            }

            let mut file = match load_serialized_file_for_scan(path) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(registry) = registry.clone() {
                file.set_type_tree_registry(Some(registry));
            }

            processed_any = true;
            let source_key = BinarySource::path(path);
            scan_pptr_scan_file(
                &source_key,
                unity_asset::environment::BinarySourceKind::SerializedFile,
                None,
                &file,
                class_id,
                has_name_filter,
                &name_lc,
                include_no_typetree,
                json,
                &mut remaining,
            )?;
        }
    }

    Ok(processed_any)
}

#[allow(clippy::too_many_arguments)]
fn scan_pptr_env_fallback(
    input: PathBuf,
    kind: String,
    source: Option<PathBuf>,
    asset_index: Option<usize>,
    class_id: Vec<i32>,
    name: String,
    limit: Option<usize>,
    include_no_typetree: bool,
    json: bool,
    strict: bool,
    show_warnings: bool,
    typetree_registries: &[PathBuf],
) -> Result<()> {
    let mut env = build_environment(strict, show_warnings, typetree_registries)?;
    load_environment_input(&mut env, &input)?;

    let kind_lc = kind.to_ascii_lowercase();
    let scan_bundles = kind_lc == "all" || kind_lc == "bundle";
    let scan_serialized = kind_lc == "all" || kind_lc == "serialized";
    if !scan_bundles && !scan_serialized {
        anyhow::bail!("Invalid --kind: {} (expected all|bundle|serialized)", kind);
    }

    let name_lc = name.to_ascii_lowercase();
    let has_name_filter = !name_lc.is_empty();

    let mut remaining = limit.unwrap_or(usize::MAX);

    let requested_source = source.as_ref().map(BinarySource::path);
    let resolved_bundle_source = if scan_bundles {
        requested_source
            .as_ref()
            .map(|req| {
                resolve_loaded_source(
                    &env,
                    unity_asset::environment::BinarySourceKind::AssetBundle,
                    req,
                )
            })
            .transpose()?
    } else {
        None
    };
    let resolved_serialized_source = if scan_serialized {
        requested_source
            .as_ref()
            .map(|req| {
                resolve_loaded_source(
                    &env,
                    unity_asset::environment::BinarySourceKind::SerializedFile,
                    req,
                )
            })
            .transpose()?
    } else {
        None
    };

    if scan_bundles {
        for (bundle_key, bundle) in env.bundles() {
            if remaining == 0 {
                break;
            }
            if let Some(resolved) = &resolved_bundle_source {
                if resolved != bundle_key {
                    continue;
                }
            }

            for (idx, file) in bundle.assets.iter().enumerate() {
                if remaining == 0 {
                    break;
                }
                if let Some(filter_idx) = asset_index {
                    if filter_idx != idx {
                        continue;
                    }
                }
                scan_pptr_scan_file(
                    bundle_key,
                    unity_asset::environment::BinarySourceKind::AssetBundle,
                    Some(idx),
                    file,
                    &class_id,
                    has_name_filter,
                    &name_lc,
                    include_no_typetree,
                    json,
                    &mut remaining,
                )?;
            }
        }
    }

    if scan_serialized {
        for (asset_key, file) in env.binary_assets() {
            if remaining == 0 {
                break;
            }
            if let Some(resolved) = &resolved_serialized_source {
                if resolved != asset_key {
                    continue;
                }
            }
            scan_pptr_scan_file(
                asset_key,
                unity_asset::environment::BinarySourceKind::SerializedFile,
                None,
                file,
                &class_id,
                has_name_filter,
                &name_lc,
                include_no_typetree,
                json,
                &mut remaining,
            )?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    input: PathBuf,
    kind: String,
    source: Option<PathBuf>,
    asset_index: Option<usize>,
    class_id: Vec<i32>,
    name: String,
    limit: Option<usize>,
    include_no_typetree: bool,
    json: bool,
    ctx: &AppContext,
) -> Result<()> {
    if let Ok(true) = scan_pptr_fast(
        &input,
        &kind,
        source.as_ref(),
        asset_index,
        &class_id,
        &name,
        limit,
        include_no_typetree,
        json,
        ctx.show_warnings,
        ctx.typetree_registries(),
    ) {
        return Ok(());
    }

    scan_pptr_env_fallback(
        input,
        kind,
        source,
        asset_index,
        class_id,
        name,
        limit,
        include_no_typetree,
        json,
        ctx.strict,
        ctx.show_warnings,
        ctx.typetree_registries(),
    )
}
