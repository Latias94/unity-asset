use crate::shared::{AppContext, class_name_for_id, load_environment_input, resolve_loaded_source};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use unity_asset::environment::{BinaryObjectKey, BinarySource, BinarySourceKind, Environment};
use unity_asset_binary::asset::SerializedFile;

#[derive(Debug, Serialize)]
struct ListObjectRecord {
    key: String,
    source: String,
    source_kind: String,
    asset_index: Option<usize>,
    path_id: i64,
    class_id: i32,
    class_name: String,
    byte_size: u32,
    name: Option<String>,
    typetree: bool,
}

fn best_effort_class_name(file: &SerializedFile, class_id: i32) -> String {
    if let Some(t) = file.find_type(class_id) {
        if let Some(root) = t.type_tree.nodes.first() {
            if !root.type_name.is_empty() {
                return root.type_name.clone();
            }
        }
        if !t.class_name.is_empty() {
            return t.class_name.clone();
        }
    }
    class_name_for_id(class_id).to_string()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    input: PathBuf,
    kind: String,
    source: Option<PathBuf>,
    asset_index: Option<usize>,
    class_id: Vec<i32>,
    class_name: String,
    name: String,
    limit: Option<usize>,
    json: bool,
    ctx: &AppContext,
) -> Result<()> {
    let mut env =
        crate::shared::build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;
    load_environment_input(&mut env, &input)?;

    let kind_lc = kind.to_ascii_lowercase();
    let want_bundle = kind_lc == "all" || kind_lc == "bundle";
    let want_serialized = kind_lc == "all" || kind_lc == "serialized";
    if !want_bundle && !want_serialized {
        anyhow::bail!("Unknown --kind: {} (expected: all|bundle|serialized)", kind);
    }

    let name_lc = name.to_ascii_lowercase();
    let class_name_lc = class_name.to_ascii_lowercase();

    let mut printed = 0usize;
    let limit = limit.unwrap_or(usize::MAX);

    if want_serialized {
        list_serialized(
            &env,
            &input,
            source.as_ref(),
            &class_id,
            &class_name_lc,
            &name_lc,
            limit,
            json,
            &mut printed,
        )?;
    }

    if printed < limit && want_bundle {
        list_bundles(
            &env,
            &input,
            source.as_ref(),
            asset_index,
            &class_id,
            &class_name_lc,
            &name_lc,
            limit,
            json,
            &mut printed,
        )?;
    }

    Ok(())
}

fn matches_filters(
    class_id_filter: &[i32],
    class_name_lc: &str,
    name_lc: &str,
    class_id: i32,
    class_name: &str,
    peek_name: Option<&str>,
) -> bool {
    if !class_id_filter.is_empty() && !class_id_filter.contains(&class_id) {
        return false;
    }
    if !class_name_lc.is_empty() && !class_name.to_ascii_lowercase().contains(class_name_lc) {
        return false;
    }
    if name_lc.is_empty() {
        return true;
    }
    peek_name
        .map(|n| n.to_ascii_lowercase().contains(name_lc))
        .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn list_serialized(
    env: &Environment,
    input: &PathBuf,
    source: Option<&PathBuf>,
    class_id_filter: &[i32],
    class_name_lc: &str,
    name_lc: &str,
    limit: usize,
    json: bool,
    printed: &mut usize,
) -> Result<()> {
    let sources: Vec<BinarySource> = if let Some(source) = source {
        let resolved = resolve_loaded_source(
            env,
            BinarySourceKind::SerializedFile,
            &BinarySource::path(source),
        )?;
        vec![resolved]
    } else {
        env.binary_assets().keys().cloned().collect()
    };

    for src in sources {
        let Some(file) = env.binary_assets().get(&src) else {
            continue;
        };

        for handle in file.object_handles() {
            if *printed >= limit {
                return Ok(());
            }

            let class_id = handle.class_id();
            let class_name = best_effort_class_name(file, class_id);
            let has_typetree = file
                .find_type(class_id)
                .map(|t| t.has_type_tree())
                .unwrap_or(false);
            let peek = handle.peek_name().ok().flatten();

            if !matches_filters(
                class_id_filter,
                class_name_lc,
                name_lc,
                class_id,
                &class_name,
                peek.as_deref(),
            ) {
                continue;
            }

            let key = BinaryObjectKey {
                source: src.clone(),
                source_kind: BinarySourceKind::SerializedFile,
                asset_index: None,
                path_id: handle.path_id(),
            };

            let record = ListObjectRecord {
                key: key.to_string(),
                source: src.to_string(),
                source_kind: "serialized".to_string(),
                asset_index: None,
                path_id: handle.path_id(),
                class_id,
                class_name,
                byte_size: handle.byte_size(),
                name: peek,
                typetree: has_typetree,
            };

            if json {
                println!("{}", serde_json::to_string(&record)?);
            } else {
                println!(
                    "{} class_id={} class={} path_id={} byte_size={} name={}",
                    record.key,
                    record.class_id,
                    record.class_name,
                    record.path_id,
                    record.byte_size,
                    record
                        .name
                        .as_deref()
                        .map(|s| format!("{:?}", s))
                        .unwrap_or_else(|| "null".to_string())
                );
            }

            *printed += 1;
        }
    }

    if *printed == 0
        && !input.is_file()
        && source.is_none()
        && env.binary_assets().is_empty()
        && !json
    {
        println!("⚠ No SerializedFiles found in {:?}", input);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn list_bundles(
    env: &Environment,
    input: &PathBuf,
    source: Option<&PathBuf>,
    asset_index: Option<usize>,
    class_id_filter: &[i32],
    class_name_lc: &str,
    name_lc: &str,
    limit: usize,
    json: bool,
    printed: &mut usize,
) -> Result<()> {
    let sources: Vec<BinarySource> = if let Some(source) = source {
        let resolved = resolve_loaded_source(
            env,
            BinarySourceKind::AssetBundle,
            &BinarySource::path(source),
        )?;
        vec![resolved]
    } else {
        env.bundles().keys().cloned().collect()
    };

    for src in sources {
        let Some(bundle) = env.bundles().get(&src) else {
            continue;
        };

        for (idx, asset) in bundle.assets.iter().enumerate() {
            if *printed >= limit {
                return Ok(());
            }
            if let Some(filter_idx) = asset_index {
                if idx != filter_idx {
                    continue;
                }
            }

            for handle in asset.object_handles() {
                if *printed >= limit {
                    return Ok(());
                }

                let class_id = handle.class_id();
                let class_name = best_effort_class_name(asset, class_id);
                let has_typetree = asset
                    .find_type(class_id)
                    .map(|t| t.has_type_tree())
                    .unwrap_or(false);
                let peek = handle.peek_name().ok().flatten();

                if !matches_filters(
                    class_id_filter,
                    class_name_lc,
                    name_lc,
                    class_id,
                    &class_name,
                    peek.as_deref(),
                ) {
                    continue;
                }

                let key = BinaryObjectKey {
                    source: src.clone(),
                    source_kind: BinarySourceKind::AssetBundle,
                    asset_index: Some(idx),
                    path_id: handle.path_id(),
                };

                let record = ListObjectRecord {
                    key: key.to_string(),
                    source: src.to_string(),
                    source_kind: "bundle".to_string(),
                    asset_index: Some(idx),
                    path_id: handle.path_id(),
                    class_id,
                    class_name,
                    byte_size: handle.byte_size(),
                    name: peek,
                    typetree: has_typetree,
                };

                if json {
                    println!("{}", serde_json::to_string(&record)?);
                } else {
                    println!(
                        "{} class_id={} class={} asset_index={} path_id={} byte_size={} name={}",
                        record.key,
                        record.class_id,
                        record.class_name,
                        idx,
                        record.path_id,
                        record.byte_size,
                        record
                            .name
                            .as_deref()
                            .map(|s| format!("{:?}", s))
                            .unwrap_or_else(|| "null".to_string())
                    );
                }

                *printed += 1;
            }
        }
    }

    if *printed == 0 && !input.is_file() && source.is_none() && env.bundles().is_empty() && !json {
        println!("⚠ No AssetBundles found in {:?}", input);
    }

    Ok(())
}
