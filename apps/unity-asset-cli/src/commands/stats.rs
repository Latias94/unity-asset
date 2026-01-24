use crate::shared::{AppContext, build_environment, load_environment_input, resolve_loaded_source};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use unity_asset::environment::{BinarySource, BinarySourceKind, Environment};

#[derive(Debug, Serialize)]
struct StatsRecord {
    source: String,
    source_kind: String,
    asset_index: Option<usize>,
    name: Option<String>,

    serialized_version: u32,
    unity_version: String,
    target_platform: i32,
    enable_type_tree: bool,
    big_id_enabled: bool,

    object_count: usize,
    type_count: usize,
    script_type_count: usize,
    external_count: usize,
    ref_type_count: usize,
}

pub(crate) fn run(
    input: &PathBuf,
    kind: &str,
    limit: &Option<usize>,
    json: &bool,
    ctx: &AppContext,
) -> Result<()> {
    let mut env = build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;
    load_environment_input(&mut env, input)?;

    let k = kind.to_ascii_lowercase();
    let limit = limit.unwrap_or(usize::MAX);

    let mut printed = 0usize;
    if k == "all" || k == "serialized" {
        printed += stats_serialized_files(&env, input, None, limit.saturating_sub(printed), *json)?;
        if printed >= limit {
            return Ok(());
        }
    }

    if k == "all" || k == "bundle" {
        printed += stats_bundles(&env, input, None, limit.saturating_sub(printed), *json)?;
    }

    if printed == 0 && !json {
        println!("No binary sources found in {:?}", input);
    }

    Ok(())
}

fn stats_serialized_files(
    env: &Environment,
    _input: &PathBuf,
    source: Option<&PathBuf>,
    limit: usize,
    json: bool,
) -> Result<usize> {
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

    let mut printed = 0usize;
    for src in sources {
        if printed >= limit {
            return Ok(printed);
        }
        let Some(file) = env.binary_assets().get(&src) else {
            continue;
        };

        let record = StatsRecord {
            source: src.to_string(),
            source_kind: "serialized".to_string(),
            asset_index: None,
            name: None,
            serialized_version: file.header.version,
            unity_version: file.unity_version.clone(),
            target_platform: file.target_platform,
            enable_type_tree: file.enable_type_tree,
            big_id_enabled: file.big_id_enabled,
            object_count: file.objects.len(),
            type_count: file.types.len(),
            script_type_count: file.script_types.len(),
            external_count: file.externals.len(),
            ref_type_count: file.ref_types.len(),
        };

        if json {
            println!("{}", serde_json::to_string(&record)?);
        } else {
            println!(
                "{} version={} unity={} objects={} types={}",
                record.source,
                record.serialized_version,
                record.unity_version,
                record.object_count,
                record.type_count
            );
        }
        printed += 1;
    }

    Ok(printed)
}

fn stats_bundles(
    env: &Environment,
    _input: &PathBuf,
    source: Option<&PathBuf>,
    limit: usize,
    json: bool,
) -> Result<usize> {
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

    let mut printed = 0usize;
    for src in sources {
        let Some(bundle) = env.bundles().get(&src) else {
            continue;
        };

        for (asset_index, file) in bundle.assets.iter().enumerate() {
            if printed >= limit {
                return Ok(printed);
            }

            let record = StatsRecord {
                source: src.to_string(),
                source_kind: "bundle".to_string(),
                asset_index: Some(asset_index),
                name: bundle.asset_names.get(asset_index).cloned(),
                serialized_version: file.header.version,
                unity_version: file.unity_version.clone(),
                target_platform: file.target_platform,
                enable_type_tree: file.enable_type_tree,
                big_id_enabled: file.big_id_enabled,
                object_count: file.objects.len(),
                type_count: file.types.len(),
                script_type_count: file.script_types.len(),
                external_count: file.externals.len(),
                ref_type_count: file.ref_types.len(),
            };

            if json {
                println!("{}", serde_json::to_string(&record)?);
            } else {
                let name = record.name.as_deref().unwrap_or("<unknown>");
                println!(
                    "{} asset_index={} name={} version={} unity={} objects={} types={}",
                    record.source,
                    asset_index,
                    name,
                    record.serialized_version,
                    record.unity_version,
                    record.object_count,
                    record.type_count
                );
            }
            printed += 1;
        }
    }

    Ok(printed)
}
