use crate::shared::{AppContext, build_environment, load_environment_input};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use unity_asset_binary::typetree::TypeTree;

#[derive(Debug, Serialize)]
struct TypeTreeRegistryDump {
    schema: u32,
    entries: Vec<TypeTreeRegistryDumpEntry>,
}

#[derive(Debug, Serialize)]
struct TypeTreeRegistryDumpEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    unity_version: Option<String>,
    class_id: i32,
    type_tree: TypeTree,
}

fn major_minor_version_pattern(unity_version: &str) -> Option<String> {
    let mut it = unity_version.split('.');
    let major = it.next()?;
    let minor = it.next()?;
    Some(format!("{major}.{minor}.*"))
}

pub(crate) fn run(
    input: PathBuf,
    output: PathBuf,
    class_id: Vec<i32>,
    version_prefix: bool,
    overwrite: bool,
    ctx: &AppContext,
) -> Result<()> {
    if output.exists() && !overwrite {
        anyhow::bail!(
            "Output already exists: {:?} (pass --overwrite to replace)",
            output
        );
    }

    let mut env = build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;
    load_environment_input(&mut env, &input)?;

    let class_filter: Option<HashSet<i32>> = if class_id.is_empty() {
        None
    } else {
        Some(class_id.into_iter().collect())
    };

    let mut entries: Vec<TypeTreeRegistryDumpEntry> = Vec::new();
    let mut seen: HashSet<(String, i32)> = HashSet::new();

    let mut files: Vec<&unity_asset_binary::asset::SerializedFile> = Vec::new();
    for file in env.binary_assets().values() {
        files.push(file);
    }
    for bundle in env.bundles().values() {
        for file in &bundle.assets {
            files.push(file);
        }
    }

    for file in files {
        if !file.enable_type_tree {
            continue;
        }
        let version_raw = file.unity_version.clone();
        let version_out = if version_prefix {
            major_minor_version_pattern(&version_raw).unwrap_or(version_raw)
        } else {
            version_raw
        };

        for t in &file.types {
            if let Some(filter) = class_filter.as_ref() {
                if !filter.contains(&t.class_id) {
                    continue;
                }
            }

            if t.type_tree.is_empty() {
                continue;
            }

            let key = (version_out.clone(), t.class_id);
            if !seen.insert(key) {
                continue;
            }

            entries.push(TypeTreeRegistryDumpEntry {
                unity_version: Some(version_out.clone()),
                class_id: t.class_id,
                type_tree: t.type_tree.clone(),
            });
        }
    }

    entries.sort_by(|a, b| {
        a.unity_version
            .as_deref()
            .unwrap_or_default()
            .cmp(b.unity_version.as_deref().unwrap_or_default())
            .then_with(|| a.class_id.cmp(&b.class_id))
    });

    let dump = TypeTreeRegistryDump { schema: 1, entries };
    let text = serde_json::to_string_pretty(&dump)?;
    std::fs::write(&output, text)?;
    println!(
        "Wrote TypeTree registry: {:?} (entries={})",
        output,
        dump.entries.len()
    );
    Ok(())
}
