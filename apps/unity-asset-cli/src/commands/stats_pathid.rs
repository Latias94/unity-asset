use crate::shared::{AppContext, build_environment, load_environment_input, resolve_loaded_source};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use unity_asset::environment::{BinarySource, BinarySourceKind, Environment};
use unity_asset_binary::asset::SerializedFile;

#[derive(Debug, Default, Clone, Copy)]
struct PathIdCounts {
    negative: usize,
    zero: usize,
    positive: usize,
}

#[derive(Debug, Default)]
struct PathIdStats {
    files_scanned: usize,
    objects_total: usize,
    counts: PathIdCounts,
    min: Option<i64>,
    max: Option<i64>,
    files_with_duplicates: usize,
    duplicate_path_ids: usize,
}

#[derive(Debug, Serialize)]
struct PathIdStatsJson {
    files_scanned: usize,
    objects_total: usize,
    negative: usize,
    zero: usize,
    positive: usize,
    min: Option<i64>,
    max: Option<i64>,
    files_with_duplicates: usize,
    duplicate_path_ids: usize,
}

impl PathIdStats {
    fn add_file(&mut self, file: &SerializedFile, check_duplicates: bool) {
        self.files_scanned += 1;

        let mut file_duplicate_count = 0usize;
        let mut seen: Option<HashSet<i64>> = if check_duplicates {
            Some(HashSet::with_capacity(file.objects.len().saturating_mul(2)))
        } else {
            None
        };

        for obj in &file.objects {
            self.objects_total += 1;
            let pid = obj.path_id;
            match pid.cmp(&0) {
                std::cmp::Ordering::Less => self.counts.negative += 1,
                std::cmp::Ordering::Equal => self.counts.zero += 1,
                std::cmp::Ordering::Greater => self.counts.positive += 1,
            }

            self.min = Some(self.min.map_or(pid, |m| m.min(pid)));
            self.max = Some(self.max.map_or(pid, |m| m.max(pid)));

            if let Some(ref mut set) = seen {
                if !set.insert(pid) {
                    file_duplicate_count += 1;
                }
            }
        }

        if check_duplicates && file_duplicate_count > 0 {
            self.files_with_duplicates += 1;
            self.duplicate_path_ids += file_duplicate_count;
        }
    }

    fn to_json(&self) -> PathIdStatsJson {
        PathIdStatsJson {
            files_scanned: self.files_scanned,
            objects_total: self.objects_total,
            negative: self.counts.negative,
            zero: self.counts.zero,
            positive: self.counts.positive,
            min: self.min,
            max: self.max,
            files_with_duplicates: self.files_with_duplicates,
            duplicate_path_ids: self.duplicate_path_ids,
        }
    }
}

pub(crate) fn run(
    input: PathBuf,
    kind: String,
    limit: Option<usize>,
    check_duplicates: bool,
    json: bool,
    ctx: &AppContext,
) -> Result<()> {
    let mut env = build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;
    load_environment_input(&mut env, &input)?;

    let k = kind.to_ascii_lowercase();
    let mut remaining = limit.unwrap_or(usize::MAX);
    let mut stats = PathIdStats::default();

    if k == "all" || k == "serialized" {
        let used = visit_serialized_files(&env, None, remaining, |file| {
            stats.add_file(file, check_duplicates);
            Ok(())
        })?;
        remaining = remaining.saturating_sub(used);
    }

    if remaining > 0 && (k == "all" || k == "bundle") {
        let used = visit_bundle_serialized_files(&env, None, remaining, |file| {
            stats.add_file(file, check_duplicates);
            Ok(())
        })?;
        let _ = used;
    }

    if stats.files_scanned == 0 {
        if json {
            println!("{}", serde_json::to_string(&stats.to_json())?);
        } else {
            println!("No binary sources found in {:?}", input);
        }
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string(&stats.to_json())?);
        return Ok(());
    }

    println!("files_scanned={}", stats.files_scanned);
    println!("objects_total={}", stats.objects_total);
    println!(
        "path_id: negative={} zero={} positive={}",
        stats.counts.negative, stats.counts.zero, stats.counts.positive
    );
    println!("path_id: min={:?} max={:?}", stats.min, stats.max);
    if check_duplicates {
        println!(
            "duplicates: files_with_duplicates={} duplicate_path_ids={}",
            stats.files_with_duplicates, stats.duplicate_path_ids
        );
    }

    Ok(())
}

fn visit_serialized_files(
    env: &Environment,
    source: Option<&PathBuf>,
    limit: usize,
    mut on_file: impl FnMut(&SerializedFile) -> Result<()>,
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

    let mut scanned = 0usize;
    for src in sources {
        if scanned >= limit {
            return Ok(scanned);
        }
        let Some(file) = env.binary_assets().get(&src) else {
            continue;
        };
        on_file(file)?;
        scanned += 1;
    }

    Ok(scanned)
}

fn visit_bundle_serialized_files(
    env: &Environment,
    source: Option<&PathBuf>,
    limit: usize,
    mut on_file: impl FnMut(&SerializedFile) -> Result<()>,
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

    let mut scanned = 0usize;
    for src in sources {
        let Some(bundle) = env.bundles().get(&src) else {
            continue;
        };

        for file in &bundle.assets {
            if scanned >= limit {
                return Ok(scanned);
            }
            on_file(file)?;
            scanned += 1;
        }
    }

    Ok(scanned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_tracks_min_max_and_signs() {
        let mut stats = PathIdStats::default();

        for v in [-3i64, -1, 0, 2, 7] {
            stats.objects_total += 1;
            match v.cmp(&0) {
                std::cmp::Ordering::Less => stats.counts.negative += 1,
                std::cmp::Ordering::Equal => stats.counts.zero += 1,
                std::cmp::Ordering::Greater => stats.counts.positive += 1,
            }
            stats.min = Some(stats.min.map_or(v, |m| m.min(v)));
            stats.max = Some(stats.max.map_or(v, |m| m.max(v)));
        }

        assert_eq!(stats.counts.negative, 2);
        assert_eq!(stats.counts.zero, 1);
        assert_eq!(stats.counts.positive, 2);
        assert_eq!(stats.min, Some(-3));
        assert_eq!(stats.max, Some(7));
    }
}
