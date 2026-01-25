use crate::shared::{AppContext, build_environment, load_environment_input, resolve_loaded_source};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use unity_asset::environment::{BinarySource, BinarySourceKind, Environment};

#[derive(Debug, Serialize)]
struct StatsRecord {
    source: String,
    source_kind: String,

    bundle_signature: Option<String>,
    bundle_version: Option<u32>,
    bundle_unity_version: Option<String>,
    bundle_unity_revision: Option<String>,
    bundle_flags: Option<u32>,
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
    summary: &bool,
    json: &bool,
    ctx: &AppContext,
) -> Result<()> {
    let mut env = build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;
    load_environment_input(&mut env, input)?;

    let k = kind.to_ascii_lowercase();
    let limit = limit.unwrap_or(usize::MAX);
    let summary = *summary;

    let mut scanned = 0usize;
    if summary {
        let mut agg = StatsSummary::default();

        if k == "all" || k == "serialized" {
            scanned += visit_serialized_files(&env, None, limit.saturating_sub(scanned), |r| {
                agg.add(&r);
                Ok(())
            })?;
        }

        if scanned < limit && (k == "all" || k == "bundle") {
            scanned += visit_bundles(&env, None, limit.saturating_sub(scanned), |r| {
                agg.add(&r);
                Ok(())
            })?;
        }

        if scanned == 0 && !json {
            println!("No binary sources found in {:?}", input);
        } else {
            agg.print(*json)?;
        }

        return Ok(());
    }

    if k == "all" || k == "serialized" {
        scanned += stats_serialized_files(&env, None, limit.saturating_sub(scanned), *json)?;
    }

    if scanned < limit && (k == "all" || k == "bundle") {
        scanned += stats_bundles(&env, None, limit.saturating_sub(scanned), *json)?;
    }

    if scanned == 0 && !json {
        println!("No binary sources found in {:?}", input);
    }

    Ok(())
}

#[derive(Default)]
struct StatsSummary {
    total: usize,
    by_source_kind: BTreeMap<String, usize>,
    by_bundle_signature: BTreeMap<String, usize>,
    by_bundle_flags: BTreeMap<String, usize>,
    by_serialized_version: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
struct StatsSummaryLine {
    group: String,
    key: String,
    count: usize,
}

impl StatsSummary {
    fn add(&mut self, record: &StatsRecord) {
        self.total += 1;

        *self
            .by_source_kind
            .entry(record.source_kind.clone())
            .or_insert(0) += 1;

        if let Some(sig) = record.bundle_signature.as_ref() {
            *self.by_bundle_signature.entry(sig.clone()).or_insert(0) += 1;
        }

        if let Some(flags) = record.bundle_flags {
            let sig = record.bundle_signature.as_deref().unwrap_or("<unknown>");
            let key = format!("{sig} flags=0x{flags:08X}");
            *self.by_bundle_flags.entry(key).or_insert(0) += 1;
        }

        let v_key = format!("{} v{}", record.source_kind, record.serialized_version);
        *self.by_serialized_version.entry(v_key).or_insert(0) += 1;
    }

    fn print(&self, json: bool) -> Result<()> {
        if json {
            println!(
                "{}",
                serde_json::to_string(&StatsSummaryLine {
                    group: "total".to_string(),
                    key: "all".to_string(),
                    count: self.total,
                })?
            );

            for (k, v) in &self.by_source_kind {
                println!(
                    "{}",
                    serde_json::to_string(&StatsSummaryLine {
                        group: "source_kind".to_string(),
                        key: k.clone(),
                        count: *v,
                    })?
                );
            }

            for (k, v) in &self.by_bundle_signature {
                println!(
                    "{}",
                    serde_json::to_string(&StatsSummaryLine {
                        group: "bundle_signature".to_string(),
                        key: k.clone(),
                        count: *v,
                    })?
                );
            }

            for (k, v) in &self.by_bundle_flags {
                println!(
                    "{}",
                    serde_json::to_string(&StatsSummaryLine {
                        group: "bundle_flags".to_string(),
                        key: k.clone(),
                        count: *v,
                    })?
                );
            }

            for (k, v) in &self.by_serialized_version {
                println!(
                    "{}",
                    serde_json::to_string(&StatsSummaryLine {
                        group: "serialized_version".to_string(),
                        key: k.clone(),
                        count: *v,
                    })?
                );
            }

            return Ok(());
        }

        println!("total={}", self.total);
        if !self.by_source_kind.is_empty() {
            println!("by_source_kind:");
            for (k, v) in &self.by_source_kind {
                println!("  {}: {}", k, v);
            }
        }
        if !self.by_bundle_signature.is_empty() {
            println!("by_bundle_signature:");
            for (k, v) in &self.by_bundle_signature {
                println!("  {}: {}", k, v);
            }
        }
        if !self.by_bundle_flags.is_empty() {
            println!("by_bundle_flags:");
            for (k, v) in &self.by_bundle_flags {
                println!("  {}: {}", k, v);
            }
        }
        if !self.by_serialized_version.is_empty() {
            println!("by_serialized_version:");
            for (k, v) in &self.by_serialized_version {
                println!("  {}: {}", k, v);
            }
        }

        Ok(())
    }
}

fn visit_serialized_files(
    env: &Environment,
    source: Option<&PathBuf>,
    limit: usize,
    mut on_record: impl FnMut(StatsRecord) -> Result<()>,
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
            bundle_signature: None,
            bundle_version: None,
            bundle_unity_version: None,
            bundle_unity_revision: None,
            bundle_flags: None,
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

        on_record(record)?;
        printed += 1;
    }

    Ok(printed)
}

fn visit_bundles(
    env: &Environment,
    source: Option<&PathBuf>,
    limit: usize,
    mut on_record: impl FnMut(StatsRecord) -> Result<()>,
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

        let bundle_signature = bundle.header.signature.clone();
        let bundle_version = bundle.header.version;
        let bundle_unity_version = Some(bundle.header.unity_version.clone());
        let bundle_unity_revision = Some(bundle.header.unity_revision.clone());
        let bundle_flags = Some(bundle.header.flags);

        for (asset_index, file) in bundle.assets.iter().enumerate() {
            if printed >= limit {
                return Ok(printed);
            }

            let record = StatsRecord {
                source: src.to_string(),
                source_kind: "bundle".to_string(),
                bundle_signature: Some(bundle_signature.clone()),
                bundle_version: Some(bundle_version),
                bundle_unity_version: bundle_unity_version.clone(),
                bundle_unity_revision: bundle_unity_revision.clone(),
                bundle_flags,
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

            on_record(record)?;
            printed += 1;
        }
    }

    Ok(printed)
}

fn stats_serialized_files(
    env: &Environment,
    source: Option<&PathBuf>,
    limit: usize,
    json: bool,
) -> Result<usize> {
    visit_serialized_files(env, source, limit, |record| {
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
        Ok(())
    })
}

fn stats_bundles(
    env: &Environment,
    source: Option<&PathBuf>,
    limit: usize,
    json: bool,
) -> Result<usize> {
    visit_bundles(env, source, limit, |record| {
        if json {
            println!("{}", serde_json::to_string(&record)?);
        } else {
            let name = record.name.as_deref().unwrap_or("<unknown>");
            let signature = record.bundle_signature.as_deref().unwrap_or("<unknown>");
            let bundle_version = record
                .bundle_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let bundle_engine = record
                .bundle_unity_revision
                .as_deref()
                .or(record.bundle_unity_version.as_deref())
                .unwrap_or("<unknown>");
            let flags = record
                .bundle_flags
                .map(|v| format!("0x{v:08X}"))
                .unwrap_or_else(|| "<unknown>".to_string());
            println!(
                "{} sig={} bundle_ver={} engine={} flags={} asset_index={} name={} version={} unity={} objects={} types={}",
                record.source,
                signature,
                bundle_version,
                bundle_engine,
                flags,
                record.asset_index.unwrap_or(0),
                name,
                record.serialized_version,
                record.unity_version,
                record.object_count,
                record.type_count
            );
        }
        Ok(())
    })
}
