use crate::pattern::container_asset_path_matches_ci;
use crate::shared::{
    AppContext, binary_object_key_for_address, build_environment, class_name_for_id,
    load_environment_input, lookup_object_type_info, object_address_for_key,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(feature = "decode")]
use unity_asset::UnityValue;
use unity_asset::environment::{BinaryObjectKey, BinarySource, Environment};
use unity_asset::{AssetLoadBudget, AssetLoadLimits, ObjectAddress};
#[cfg(feature = "decode")]
use unity_asset_binary::object::UnityObject;

#[cfg(feature = "decode")]
use unity_asset_binary::{asset::class_ids, unity_version::UnityVersion};

#[cfg(feature = "decode")]
use unity_asset_decode::{
    audio::{AudioClipConverter, AudioProcessor},
    sprite::SpriteProcessor,
    texture::{TextureExporter, TextureProcessor},
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    input: PathBuf,
    output: PathBuf,
    pattern: String,
    limit: Option<usize>,
    class_ids: Vec<i32>,
    class_name: String,
    dry_run: bool,
    decode: bool,
    overwrite: bool,
    skip_existing: bool,
    manifest: Option<PathBuf>,
    resume: Option<PathBuf>,
    retry_failed_from: Option<PathBuf>,
    continue_on_error: bool,
    jobs: usize,
    ctx: &AppContext,
) -> Result<()> {
    export_bundle_command(
        input,
        output,
        pattern,
        limit,
        class_ids,
        class_name,
        dry_run,
        decode,
        overwrite,
        skip_existing,
        manifest,
        resume,
        retry_failed_from,
        continue_on_error,
        jobs,
        ctx.strict,
        ctx.show_warnings,
        ctx.typetree_registries(),
    )
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

#[cfg(feature = "decode")]
fn text_asset_bytes(obj: &UnityObject) -> Vec<u8> {
    if let Some(UnityValue::String(s)) = obj.get("m_Script")
        && !s.is_empty()
    {
        return s.as_bytes().to_vec();
    }

    if let Some(UnityValue::String(s)) = obj.get("m_Text")
        && !s.is_empty()
    {
        return s.as_bytes().to_vec();
    }

    for key in ["m_Bytes", "m_Data"] {
        match obj.get(key) {
            Some(UnityValue::Bytes(b)) if !b.is_empty() => return b.clone(),
            Some(UnityValue::Array(arr)) => {
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
            _ => {}
        }
    }

    Vec::new()
}

#[cfg(feature = "decode")]
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

#[derive(Debug, Clone)]
struct ExportJob {
    order: usize,
    asset_path: String,
    key: BinaryObjectKey,
    address: ObjectAddress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExportStatus {
    ExportedRaw,
    ExportedDecoded,
    SkippedExisting,
    Resumed,
    Failed,
    Planned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportManifestEntry {
    order: usize,
    asset_path: String,
    address: ObjectAddress,
    type_id: Option<i32>,
    class_name: Option<String>,
    status: ExportStatus,
    output_path: Option<String>,
    output_bytes: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportManifest {
    schema: u32,
    created_unix_ms: u128,
    input: String,
    output: String,
    pattern: String,
    decode: bool,
    overwrite: bool,
    skip_existing: bool,
    jobs: usize,
    strict: bool,
    show_warnings: bool,
    limit: Option<usize>,
    class_ids: Vec<i32>,
    class_name: String,
    planned: usize,
    exported: usize,
    skipped_unresolved: usize,
    skipped_existing: usize,
    #[serde(default)]
    resumed: usize,
    #[serde(default)]
    failed: usize,
    filtered: usize,
    entries: Vec<ExportManifestEntry>,
}

const EXPORT_MANIFEST_SCHEMA: u32 = 2;
const MAX_EXPORT_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
struct ExportOutcome {
    order: usize,
    message: String,
    did_export: bool,
    did_skip_existing: bool,
    entry: ExportManifestEntry,
}

fn failed_export_outcome(env: &Environment, job: &ExportJob, error: String) -> ExportOutcome {
    let (type_id, _) = lookup_object_type_info(env, &job.key);
    let class_name = (type_id != 0).then(|| class_name_for_id(type_id).into_owned());
    ExportOutcome {
        order: job.order,
        message: format!("FAILED {} error={}", job.asset_path, error),
        did_export: false,
        did_skip_existing: false,
        entry: ExportManifestEntry {
            order: job.order,
            asset_path: job.asset_path.clone(),
            address: job.address.clone(),
            type_id: (type_id != 0).then_some(type_id),
            class_name,
            status: ExportStatus::Failed,
            output_path: None,
            output_bytes: None,
            error: Some(error),
        },
    }
}

#[derive(Debug, Default)]
struct PathAllocator {
    reserved: Mutex<HashSet<PathBuf>>,
}

impl PathAllocator {
    fn reserve(&self, proposed: PathBuf, key: &BinaryObjectKey, overwrite: bool) -> PathBuf {
        let mut reserved = match self.reserved.lock() {
            Ok(v) => v,
            Err(e) => e.into_inner(),
        };

        if (overwrite || !proposed.exists()) && !reserved.contains(&proposed) {
            reserved.insert(proposed.clone());
            return proposed;
        }

        let base_suffix = match key.source_kind {
            unity_asset::environment::BinarySourceKind::SerializedFile => {
                format!("sf.{}", key.path_id)
            }
            unity_asset::environment::BinarySourceKind::AssetBundle => {
                format!("ab{}.{}", key.asset_index.unwrap_or_default(), key.path_id)
            }
        };

        let mut candidate = path_with_suffix(&proposed, &base_suffix);
        if (overwrite || !candidate.exists()) && !reserved.contains(&candidate) {
            reserved.insert(candidate.clone());
            return candidate;
        }

        let mut i = 1usize;
        loop {
            candidate = path_with_suffix(&proposed, &format!("{}.{}", base_suffix, i));
            if (overwrite || !candidate.exists()) && !reserved.contains(&candidate) {
                reserved.insert(candidate.clone());
                return candidate;
            }
            i += 1;
        }
    }
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|e| e.to_str());
    let file_name = match ext {
        Some(ext) => format!("{}.{}.{}", stem, suffix, ext),
        None => format!("{}.{}", stem, suffix),
    };
    parent.join(file_name)
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(feature = "decode")]
fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
}

fn write_export_manifest(path: &Path, manifest: ExportManifest) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension("tmp");
    let file = std::fs::File::create(&tmp)?;
    serde_json::to_writer_pretty(&file, &manifest)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_export_manifest(path: &Path) -> Result<ExportManifest> {
    let file = std::fs::File::open(path)?;
    deserialize_export_manifest(file, MAX_EXPORT_MANIFEST_BYTES)
}

fn deserialize_export_manifest(
    reader: impl std::io::Read,
    max_bytes: u64,
) -> Result<ExportManifest> {
    let limits = AssetLoadLimits {
        max_bytes,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(limits)?;
    Ok(budget.deserialize_json(reader)?)
}

fn ensure_unique_manifest_jobs(entries: &[ExportManifestEntry], usage: &str) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let job = (entry.asset_path.clone(), entry.address.clone());
        if !seen.insert(job) {
            anyhow::bail!(
                "Duplicate asset path/address pair in {usage} manifest: asset_path={:?}, address={}",
                entry.asset_path,
                entry.address.to_compact_string()?
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn export_bundle_command(
    input: PathBuf,
    output: PathBuf,
    pattern: String,
    limit: Option<usize>,
    class_ids: Vec<i32>,
    class_name: String,
    dry_run: bool,
    decode: bool,
    overwrite: bool,
    skip_existing: bool,
    manifest: Option<PathBuf>,
    resume: Option<PathBuf>,
    retry_failed_from: Option<PathBuf>,
    continue_on_error: bool,
    jobs: usize,
    strict: bool,
    show_warnings: bool,
    typetree_registries: &[PathBuf],
) -> Result<()> {
    let mut operation_budget = AssetLoadBudget::default();
    let mut env = build_environment(
        strict,
        show_warnings,
        typetree_registries,
        &mut operation_budget,
    )?;
    load_environment_input(&mut env, &input, &mut operation_budget)?;

    let mut resume_map: std::collections::HashMap<(String, ObjectAddress), ExportManifestEntry> =
        std::collections::HashMap::new();
    if let Some(path) = resume.as_ref() {
        let prev = read_export_manifest(path)?;
        if prev.schema != EXPORT_MANIFEST_SCHEMA {
            anyhow::bail!("Unsupported --resume manifest schema: {}", prev.schema);
        }
        ensure_unique_manifest_jobs(&prev.entries, "--resume")?;
        for e in prev.entries {
            let resume_key = (e.asset_path.clone(), e.address.clone());
            resume_map.insert(resume_key, e);
        }
    }

    let mut retry_failed_jobs: Option<Vec<ExportJob>> = None;
    if let Some(path) = retry_failed_from.as_ref() {
        let prev = read_export_manifest(path)?;
        if prev.schema != EXPORT_MANIFEST_SCHEMA {
            anyhow::bail!(
                "Unsupported --retry-failed-from manifest schema: {}",
                prev.schema
            );
        }
        ensure_unique_manifest_jobs(&prev.entries, "--retry-failed-from")?;
        let mut jobs: Vec<ExportJob> = Vec::new();
        let mut order = 0usize;
        for e in prev.entries {
            if !matches!(e.status, ExportStatus::Failed) {
                continue;
            }
            if !pattern.is_empty()
                && !e
                    .asset_path
                    .to_ascii_lowercase()
                    .contains(&pattern.to_ascii_lowercase())
            {
                continue;
            }
            let address = e.address.clone();
            let key = binary_object_key_for_address(&env, &input, &address)?;
            if key.source_kind != unity_asset::environment::BinarySourceKind::AssetBundle {
                anyhow::bail!(
                    "--retry-failed-from entry {} is not an AssetBundle address",
                    address.to_compact_string()?
                );
            }
            jobs.push(ExportJob {
                order,
                asset_path: e.asset_path,
                key,
                address,
            });
            order += 1;
        }
        retry_failed_jobs = Some(jobs);
    }

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
    if bundle_sources.is_empty() && retry_failed_from.is_none() {
        println!("⚠ No AssetBundles found in {:?}", input);
        return Ok(());
    }

    let class_name_lc = class_name.to_ascii_lowercase();
    let mut skipped = 0usize;
    let mut filtered = 0usize;
    let mut resumed = 0usize;
    let mut planned = 0usize;
    let mut order = 0usize;
    let mut export_jobs: Vec<ExportJob> = Vec::new();
    let mut pre_outcomes: Vec<ExportOutcome> = Vec::new();

    if let Some(jobs) = retry_failed_jobs.take() {
        for mut job in jobs {
            if let Some(max) = limit
                && planned >= max
            {
                break;
            }

            if !class_ids.is_empty() || !class_name_lc.is_empty() {
                let (type_id, _) = lookup_object_type_info(&env, &job.key);
                if !class_ids.is_empty() && !class_ids.contains(&type_id) {
                    filtered += 1;
                    continue;
                }
                if !class_name_lc.is_empty() {
                    let name = class_name_for_id(type_id);
                    if !name.as_ref().to_ascii_lowercase().contains(&class_name_lc) {
                        filtered += 1;
                        continue;
                    }
                }
            }

            job.order = order;
            export_jobs.push(job);
            planned += 1;
            order += 1;
        }
    } else {
        for bundle_source in bundle_sources {
            let entries =
                env.bundle_container_entries_source(&bundle_source, &mut operation_budget)?;
            let mut entries: Vec<_> = entries
                .into_iter()
                .filter(|e| container_asset_path_matches_ci(&e.asset_path, &pattern))
                .collect();
            entries.sort_by(|a, b| a.asset_path.cmp(&b.asset_path));

            for entry in entries {
                if let Some(max) = limit
                    && planned >= max
                {
                    break;
                }
                let Some(key) = entry.key else {
                    skipped += 1;
                    continue;
                };

                let address = object_address_for_key(&env, &input, &key)?;
                let resume_key = (entry.asset_path.clone(), address.clone());
                let effective_skip_existing = skip_existing || resume.is_some();
                if effective_skip_existing
                    && !overwrite
                    && let Some(prev) = resume_map.get(&resume_key)
                    && let Some(p) = prev.output_path.as_ref()
                {
                    let prev_path = PathBuf::from(p);
                    if prev_path.exists()
                        && matches!(
                            prev.status,
                            ExportStatus::ExportedRaw
                                | ExportStatus::ExportedDecoded
                                | ExportStatus::SkippedExisting
                                | ExportStatus::Resumed
                        )
                    {
                        resumed += 1;
                        planned += 1;
                        pre_outcomes.push(ExportOutcome {
                            order,
                            message: format!("↷ {} -> {:?} (resumed)", entry.asset_path, prev_path),
                            did_export: false,
                            did_skip_existing: true,
                            entry: ExportManifestEntry {
                                order,
                                asset_path: entry.asset_path.clone(),
                                address,
                                type_id: prev.type_id,
                                class_name: prev.class_name.clone(),
                                status: ExportStatus::Resumed,
                                output_path: Some(prev_path.to_string_lossy().to_string()),
                                output_bytes: prev.output_bytes,
                                error: None,
                            },
                        });
                        order += 1;
                        continue;
                    }
                }

                if !class_ids.is_empty() || !class_name_lc.is_empty() {
                    let (type_id, _) = lookup_object_type_info(&env, &key);
                    if !class_ids.is_empty() && !class_ids.contains(&type_id) {
                        filtered += 1;
                        continue;
                    }
                    if !class_name_lc.is_empty() {
                        let name = class_name_for_id(type_id);
                        if !name.as_ref().to_ascii_lowercase().contains(&class_name_lc) {
                            filtered += 1;
                            continue;
                        }
                    }
                }

                export_jobs.push(ExportJob {
                    order,
                    asset_path: entry.asset_path,
                    key,
                    address,
                });
                planned += 1;
                order += 1;
            }
        }
    }

    if export_jobs.is_empty() && pre_outcomes.is_empty() {
        if let Some(path) = manifest {
            write_export_manifest(
                &path,
                ExportManifest {
                    schema: EXPORT_MANIFEST_SCHEMA,
                    created_unix_ms: now_unix_ms(),
                    input: input.to_string_lossy().to_string(),
                    output: output.to_string_lossy().to_string(),
                    pattern,
                    decode,
                    overwrite,
                    skip_existing,
                    jobs,
                    strict,
                    show_warnings,
                    limit,
                    class_ids,
                    class_name,
                    planned: 0,
                    exported: 0,
                    skipped_unresolved: skipped,
                    skipped_existing: 0,
                    resumed: 0,
                    failed: 0,
                    filtered,
                    entries: Vec::new(),
                },
            )?;
        }
        println!(
            "Exported 0 entries, skipped {} (unresolved), filtered {}",
            skipped, filtered
        );
        return Ok(());
    }

    let allocator = Arc::new(PathAllocator::default());

    if dry_run {
        let mut manifest_entries: Vec<ExportManifestEntry> =
            Vec::with_capacity(pre_outcomes.len() + export_jobs.len());

        for o in &pre_outcomes {
            println!("DRY-RUN {}", o.message);
            manifest_entries.push(o.entry.clone());
        }

        for job in &export_jobs {
            let (type_id, _) = lookup_object_type_info(&env, &job.key);
            let class_name = if type_id == 0 {
                None
            } else {
                Some(class_name_for_id(type_id).into_owned())
            };
            let mut dest = output.join(sanitize_asset_path(&job.asset_path));
            if decode {
                if dest.extension().is_none() {
                    dest.set_extension("bin");
                }
            } else {
                dest.set_extension("bin");
            }
            if skip_existing && dest.exists() && !overwrite {
                println!("DRY-RUN {} -> SKIP(existing) {:?}", job.asset_path, dest);
                manifest_entries.push(ExportManifestEntry {
                    order: job.order,
                    asset_path: job.asset_path.clone(),
                    address: job.address.clone(),
                    type_id: if type_id == 0 { None } else { Some(type_id) },
                    class_name: class_name.clone(),
                    status: ExportStatus::SkippedExisting,
                    output_path: Some(dest.to_string_lossy().to_string()),
                    output_bytes: None,
                    error: None,
                });
                continue;
            }
            let dest = allocator.reserve(dest, &job.key, overwrite);
            println!("DRY-RUN {} -> {:?}", job.asset_path, dest);
            manifest_entries.push(ExportManifestEntry {
                order: job.order,
                asset_path: job.asset_path.clone(),
                address: job.address.clone(),
                type_id: if type_id == 0 { None } else { Some(type_id) },
                class_name,
                status: ExportStatus::Planned,
                output_path: Some(dest.to_string_lossy().to_string()),
                output_bytes: None,
                error: None,
            });
        }
        manifest_entries.sort_by_key(|e| e.order);
        if let Some(path) = manifest {
            let resumed_count = manifest_entries
                .iter()
                .filter(|e| matches!(e.status, ExportStatus::Resumed))
                .count();
            let skipped_existing_count = manifest_entries
                .iter()
                .filter(|e| matches!(e.status, ExportStatus::SkippedExisting))
                .count();
            write_export_manifest(
                &path,
                ExportManifest {
                    schema: EXPORT_MANIFEST_SCHEMA,
                    created_unix_ms: now_unix_ms(),
                    input: input.to_string_lossy().to_string(),
                    output: output.to_string_lossy().to_string(),
                    pattern,
                    decode,
                    overwrite,
                    skip_existing,
                    jobs,
                    strict,
                    show_warnings,
                    limit,
                    class_ids,
                    class_name,
                    planned: manifest_entries.len(),
                    exported: 0,
                    skipped_unresolved: skipped,
                    skipped_existing: skipped_existing_count + resumed_count,
                    resumed: resumed_count,
                    failed: 0,
                    filtered,
                    entries: manifest_entries,
                },
            )?;
        }
        println!(
            "Exported {} entries, skipped {} (unresolved), filtered {}, resumed {}",
            export_jobs.len() + pre_outcomes.len(),
            skipped,
            filtered,
            resumed
        );
        return Ok(());
    }

    #[cfg(not(feature = "decode"))]
    if decode {
        anyhow::bail!(
            "--decode requires compiling `unity-asset-cli` with feature `decode` (build with default features, or `--features decode`)."
        );
    }

    if export_jobs.is_empty() {
        let mut outcomes = pre_outcomes;
        outcomes.sort_by_key(|o| o.order);

        if let Some(path) = manifest.as_ref() {
            let mut entries: Vec<ExportManifestEntry> =
                outcomes.iter().map(|o| o.entry.clone()).collect();
            entries.sort_by_key(|e| e.order);
            write_export_manifest(
                path,
                ExportManifest {
                    schema: EXPORT_MANIFEST_SCHEMA,
                    created_unix_ms: now_unix_ms(),
                    input: input.to_string_lossy().to_string(),
                    output: output.to_string_lossy().to_string(),
                    pattern,
                    decode,
                    overwrite,
                    skip_existing,
                    jobs: 1,
                    strict,
                    show_warnings,
                    limit,
                    class_ids,
                    class_name,
                    planned,
                    exported: 0,
                    skipped_unresolved: skipped,
                    skipped_existing: resumed,
                    resumed,
                    failed: 0,
                    filtered,
                    entries,
                },
            )?;
        }

        for o in &outcomes {
            println!("{}", o.message);
        }
        println!(
            "Exported 0 entries, skipped {} (unresolved), skipped {} (existing), filtered {}, resumed {} [jobs=0]",
            skipped, resumed, filtered, resumed
        );
        return Ok(());
    }

    let threads = {
        let auto = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let requested = if jobs == 0 { auto } else { jobs.max(1) };
        requested.min(export_jobs.len()).max(1)
    };

    let env = Arc::new(env);
    let object_budget = Arc::new(Mutex::new(operation_budget));
    let export_jobs = Arc::new(export_jobs);
    let next = Arc::new(AtomicUsize::new(0));
    let abort = Arc::new(AtomicBool::new(false));
    let exported = Arc::new(AtomicUsize::new(0));
    let skipped_existing = Arc::new(AtomicUsize::new(0));
    let failed_count = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<Vec<ExportOutcome>>> = Arc::new(Mutex::new(Vec::new()));
    let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    thread::scope(|scope| {
        for _ in 0..threads {
            let env = Arc::clone(&env);
            let export_jobs = Arc::clone(&export_jobs);
            let next = Arc::clone(&next);
            let abort = Arc::clone(&abort);
            let exported = Arc::clone(&exported);
            let skipped_existing = Arc::clone(&skipped_existing);
            let failed_count = Arc::clone(&failed_count);
            let results = Arc::clone(&results);
            let first_error = Arc::clone(&first_error);
            let allocator = Arc::clone(&allocator);
            let object_budget = Arc::clone(&object_budget);
            let output = output.clone();

            scope.spawn(move || {
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }

                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= export_jobs.len() {
                        break;
                    }

                    let job = &export_jobs[idx];
                    let outcome = match export_one_entry(
                        &env,
                        &object_budget,
                        &allocator,
                        &output,
                        &job.asset_path,
                        &job.key,
                        &job.address,
                        job.order,
                        decode,
                        overwrite,
                        skip_existing,
                    ) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            failed_count.fetch_add(1, Ordering::Relaxed);
                            let error = e.to_string();
                            let outcome = failed_export_outcome(&env, job, error.clone());
                            if !continue_on_error {
                                abort.store(true, Ordering::Relaxed);
                                let mut slot = match first_error.lock() {
                                    Ok(v) => v,
                                    Err(e) => e.into_inner(),
                                };
                                if slot.is_none() {
                                    *slot = Some(error);
                                }
                            }
                            Some(outcome)
                        }
                    };

                    let Some(outcome) = outcome else {
                        break;
                    };

                    if outcome.did_export {
                        exported.fetch_add(1, Ordering::Relaxed);
                    }
                    if outcome.did_skip_existing {
                        skipped_existing.fetch_add(1, Ordering::Relaxed);
                    }
                    let mut out = match results.lock() {
                        Ok(v) => v,
                        Err(e) => e.into_inner(),
                    };
                    out.push(outcome);
                }
            });
        }
    });

    let error = match first_error.lock() {
        Ok(v) => v.clone(),
        Err(e) => e.into_inner().clone(),
    };

    let mut outcomes = match results.lock() {
        Ok(v) => v.clone(),
        Err(e) => e.into_inner().clone(),
    };
    outcomes.extend(pre_outcomes);
    outcomes.sort_by_key(|o| o.order);

    if let Some(path) = manifest.as_ref() {
        let mut entries: Vec<ExportManifestEntry> =
            outcomes.iter().map(|o| o.entry.clone()).collect();
        entries.sort_by_key(|e| e.order);
        let skipped_existing_total = skipped_existing.load(Ordering::Relaxed) + resumed;
        write_export_manifest(
            path,
            ExportManifest {
                schema: EXPORT_MANIFEST_SCHEMA,
                created_unix_ms: now_unix_ms(),
                input: input.to_string_lossy().to_string(),
                output: output.to_string_lossy().to_string(),
                pattern: pattern.clone(),
                decode,
                overwrite,
                skip_existing,
                jobs: threads,
                strict,
                show_warnings,
                limit,
                class_ids: class_ids.clone(),
                class_name: class_name.clone(),
                planned,
                exported: exported.load(Ordering::Relaxed),
                skipped_unresolved: skipped,
                skipped_existing: skipped_existing_total,
                resumed,
                failed: failed_count.load(Ordering::Relaxed),
                filtered,
                entries,
            },
        )?;
    }

    if let Some(err) = error {
        return Err(anyhow::anyhow!(err));
    }

    for o in &outcomes {
        println!("{}", o.message);
    }

    let failed = failed_count.load(Ordering::Relaxed);
    if continue_on_error && failed > 0 {
        println!(
            "Exported {} entries, skipped {} (unresolved), skipped {} (existing), filtered {}, resumed {}, failed {} [jobs={}]",
            exported.load(Ordering::Relaxed),
            skipped,
            skipped_existing.load(Ordering::Relaxed) + resumed,
            filtered,
            resumed,
            failed,
            threads,
        );
        return Err(anyhow::anyhow!(
            "{} entries failed (use --manifest to inspect, or re-run with --resume)",
            failed
        ));
    }

    println!(
        "Exported {} entries, skipped {} (unresolved), skipped {} (existing), filtered {}, resumed {}, failed {} [jobs={}]",
        exported.load(Ordering::Relaxed),
        skipped,
        skipped_existing.load(Ordering::Relaxed) + resumed,
        filtered,
        resumed,
        failed,
        threads,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn export_one_entry(
    env: &Environment,
    budget: &Mutex<AssetLoadBudget>,
    allocator: &PathAllocator,
    output: &Path,
    asset_path: &str,
    key: &BinaryObjectKey,
    address: &ObjectAddress,
    order: usize,
    decode: bool,
    overwrite: bool,
    skip_existing: bool,
) -> Result<ExportOutcome> {
    let obj = {
        let mut budget = budget
            .lock()
            .map_err(|_| anyhow::anyhow!("asset load budget lock poisoned"))?;
        env.read_binary_object_key(key, &mut budget)?
    };
    let type_id = obj.info.class_id();
    let class_name = if type_id == 0 {
        None
    } else {
        Some(obj.class_name().to_string())
    };

    if decode {
        #[cfg(feature = "decode")]
        match try_decode_export_best_effort(
            env,
            budget,
            allocator,
            output,
            asset_path,
            key,
            &obj,
            overwrite,
            skip_existing,
        ) {
            DecodeAttempt::Exported { dest, output_bytes } => {
                return Ok(ExportOutcome {
                    order,
                    message: format!(
                        "✓ {} -> {:?} (decoded, class_id={})",
                        asset_path,
                        dest,
                        obj.info.class_id()
                    ),
                    did_export: true,
                    did_skip_existing: false,
                    entry: ExportManifestEntry {
                        order,
                        asset_path: asset_path.to_string(),
                        address: address.clone(),
                        type_id: Some(type_id),
                        class_name,
                        status: ExportStatus::ExportedDecoded,
                        output_path: Some(dest.to_string_lossy().to_string()),
                        output_bytes,
                        error: None,
                    },
                });
            }
            DecodeAttempt::SkippedExisting { dest } => {
                return Ok(ExportOutcome {
                    order,
                    message: format!("↷ {} -> {:?} (skipped existing)", asset_path, dest),
                    did_export: false,
                    did_skip_existing: true,
                    entry: ExportManifestEntry {
                        order,
                        asset_path: asset_path.to_string(),
                        address: address.clone(),
                        type_id: Some(type_id),
                        class_name,
                        status: ExportStatus::SkippedExisting,
                        output_path: Some(dest.to_string_lossy().to_string()),
                        output_bytes: None,
                        error: None,
                    },
                });
            }
            DecodeAttempt::NotApplicable => {}
        }
    }

    let bytes = obj.raw_data();
    let mut dest = output.join(sanitize_asset_path(asset_path));
    dest.set_extension("bin");

    if decode && let Some(ext) = magic_based_extension(asset_path, bytes) {
        dest = output.join(sanitize_asset_path(asset_path));
        dest.set_extension(ext);
    }

    if skip_existing && dest.exists() && !overwrite {
        return Ok(ExportOutcome {
            order,
            message: format!("↷ {} -> {:?} (skipped existing)", asset_path, dest),
            did_export: false,
            did_skip_existing: true,
            entry: ExportManifestEntry {
                order,
                asset_path: asset_path.to_string(),
                address: address.clone(),
                type_id: Some(type_id),
                class_name,
                status: ExportStatus::SkippedExisting,
                output_path: Some(dest.to_string_lossy().to_string()),
                output_bytes: None,
                error: None,
            },
        });
    }

    let dest = allocator.reserve(dest, key, overwrite);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, bytes)?;

    Ok(ExportOutcome {
        order,
        message: format!(
            "✓ {} -> {:?} (raw, class_id={}, bytes={})",
            asset_path,
            dest,
            obj.info.class_id(),
            bytes.len()
        ),
        did_export: true,
        did_skip_existing: false,
        entry: ExportManifestEntry {
            order,
            asset_path: asset_path.to_string(),
            address: address.clone(),
            type_id: Some(type_id),
            class_name,
            status: ExportStatus::ExportedRaw,
            output_path: Some(dest.to_string_lossy().to_string()),
            output_bytes: Some(bytes.len() as u64),
            error: None,
        },
    })
}

#[cfg(feature = "decode")]
#[derive(Debug, Clone)]
enum DecodeAttempt {
    NotApplicable,
    Exported {
        dest: PathBuf,
        output_bytes: Option<u64>,
    },
    SkippedExisting {
        dest: PathBuf,
    },
}

#[cfg(feature = "decode")]
#[allow(clippy::too_many_arguments)]
fn try_decode_export_best_effort(
    env: &Environment,
    budget: &Mutex<AssetLoadBudget>,
    allocator: &PathAllocator,
    output: &Path,
    asset_path: &str,
    key: &BinaryObjectKey,
    obj: &UnityObject,
    overwrite: bool,
    skip_existing: bool,
) -> DecodeAttempt {
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

    match obj.info.class_id() {
        class_ids::AUDIO_CLIP => (|| -> anyhow::Result<DecodeAttempt> {
            let converter = AudioClipConverter::new(unity_version.clone());
            let clip = converter.from_unity_object(obj)?;

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
                if let Some(v) = obj.get("m_AudioData") {
                    match v {
                        UnityValue::Bytes(b) => eprintln!("  m_AudioData len: {}", b.len()),
                        UnityValue::Array(items) => eprintln!("  m_AudioData len: {}", items.len()),
                        _ => {}
                    }
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

            let mut dest = output.join(sanitize_asset_path(asset_path));
            match converter.get_audio_data(&clip) {
                Ok(audio_bytes) if !audio_bytes.is_empty() => {
                    let ext = std::path::Path::new(asset_path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or(clip.compression_format().extension())
                        .to_ascii_lowercase();
                    dest.set_extension(ext);
                    if skip_existing && dest.exists() && !overwrite {
                        return Ok(DecodeAttempt::SkippedExisting { dest });
                    }
                    let dest = allocator.reserve(dest, key, overwrite);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, &audio_bytes)?;
                    Ok(DecodeAttempt::Exported {
                        dest,
                        output_bytes: Some(audio_bytes.len() as u64),
                    })
                }
                _ => {
                    if clip.is_streamed()
                        && let Ok(bytes) = env.read_stream_data_source(
                            &key.source,
                            key.source_kind,
                            &clip.stream_info.path,
                            clip.stream_info.offset,
                            clip.stream_info.size,
                        )
                            && !bytes.is_empty() {
                                let ext = std::path::Path::new(asset_path)
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .unwrap_or(clip.compression_format().extension())
                                    .to_ascii_lowercase();
                                dest.set_extension(ext);
                                if skip_existing && dest.exists() && !overwrite {
                                    return Ok(DecodeAttempt::SkippedExisting { dest });
                                }
                                let dest = allocator.reserve(dest, key, overwrite);
                                if let Some(parent) = dest.parent() {
                                    std::fs::create_dir_all(parent)?;
                                }
                                std::fs::write(&dest, &bytes)?;
                                return Ok(DecodeAttempt::Exported {
                                    dest,
                                    output_bytes: Some(bytes.len() as u64),
                                });
                            }

                    dest.set_extension("wav");
                    if skip_existing && dest.exists() && !overwrite {
                        return Ok(DecodeAttempt::SkippedExisting { dest });
                    }
                    let dest = allocator.reserve(dest, key, overwrite);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let audio_processor = AudioProcessor::new(unity_version);
                    audio_processor.process_and_export(obj, &dest)?;
                    Ok(DecodeAttempt::Exported {
                        output_bytes: file_len(&dest),
                        dest,
                    })
                }
            }
        })()
        .unwrap_or(DecodeAttempt::NotApplicable),
        class_ids::TEXTURE_2D => (|| -> anyhow::Result<DecodeAttempt> {
            let mut dest = output.join(sanitize_asset_path(asset_path));
            dest.set_extension("png");
            if skip_existing && dest.exists() && !overwrite {
                return Ok(DecodeAttempt::SkippedExisting { dest });
            }
            let dest = allocator.reserve(dest, key, overwrite);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let texture_processor = TextureProcessor::new(unity_version);
            let mut texture = texture_processor.convert_object(obj)?;
            if texture.image_data.is_empty() && texture.is_streamed()
                && let Ok(bytes) = env.read_stream_data_source(
                    &key.source,
                    key.source_kind,
                    &texture.stream_info.path,
                    texture.stream_info.offset,
                    texture.stream_info.size,
                )
                    && !bytes.is_empty() {
                        texture.data_size = bytes.len() as i32;
                        texture.image_data = bytes;
                    }

            let image = texture_processor.decode_texture(&texture)?;
            TextureExporter::export_auto(&image, &dest)?;
            Ok(DecodeAttempt::Exported {
                output_bytes: file_len(&dest),
                dest,
            })
        })()
        .unwrap_or(DecodeAttempt::NotApplicable),
        class_ids::TEXT_ASSET => (|| -> anyhow::Result<DecodeAttempt> {
            let bytes = text_asset_bytes(obj);
            if bytes.is_empty() {
                return Ok(DecodeAttempt::NotApplicable);
            }

            let mut dest = output.join(sanitize_asset_path(asset_path));
            if dest.extension().is_none() {
                dest.set_extension(if std::str::from_utf8(&bytes).is_ok() {
                    "txt"
                } else {
                    "bin"
                });
            }
            if skip_existing && dest.exists() && !overwrite {
                return Ok(DecodeAttempt::SkippedExisting { dest });
            }
            let dest = allocator.reserve(dest, key, overwrite);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &bytes)?;
            Ok(DecodeAttempt::Exported {
                dest,
                output_bytes: Some(bytes.len() as u64),
            })
        })()
        .unwrap_or(DecodeAttempt::NotApplicable),
        class_ids::SPRITE => (|| -> anyhow::Result<DecodeAttempt> {
            let Some(obj_ref) = (match key.source_kind {
                unity_asset::environment::BinarySourceKind::AssetBundle => key
                    .asset_index
                    .and_then(|i| env.find_binary_object_in_bundle_asset_source(&key.source, i, key.path_id)),
                unity_asset::environment::BinarySourceKind::SerializedFile => {
                    env.find_binary_object_in_source_id(&key.source, key.path_id)
                }
            }) else {
                return Ok(DecodeAttempt::NotApplicable);
            };

            let sprite_processor = SpriteProcessor::new(unity_version.clone());
            let sprite = sprite_processor.parse_sprite(obj)?.sprite;

            let (file_id, texture_path_id) = if let Some((file_id, path_id)) = sprite_texture_pptr(obj) {
                (file_id, path_id)
            } else if sprite.render_data.texture_path_id != 0 {
                (0, sprite.render_data.texture_path_id)
            } else {
                return Ok(DecodeAttempt::NotApplicable);
            };

            let texture_obj = {
                let mut budget = budget
                    .lock()
                    .map_err(|_| anyhow::anyhow!("asset load budget lock poisoned"))?;
                env.read_binary_pptr(&obj_ref, file_id, texture_path_id, &mut budget)?
            };

            let texture_processor = TextureProcessor::new(unity_version);
            let mut texture = texture_processor.convert_object(&texture_obj)?;
            if texture.image_data.is_empty() && texture.is_streamed()
                && let Ok(bytes) = env.read_stream_data_source(
                    &key.source,
                    key.source_kind,
                    &texture.stream_info.path,
                    texture.stream_info.offset,
                    texture.stream_info.size,
                )
                    && !bytes.is_empty() {
                        texture.data_size = bytes.len() as i32;
                        texture.image_data = bytes;
                    }

            let png_bytes = sprite_processor.extract_sprite_image(&sprite, &texture)?;

            let mut dest = output.join(sanitize_asset_path(asset_path));
            if dest.extension().is_some() {
                let stem = dest
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("sprite");
                dest.set_file_name(format!("{}.sprite.png", stem));
            } else {
                dest.set_extension("png");
            }
            if skip_existing && dest.exists() && !overwrite {
                return Ok(DecodeAttempt::SkippedExisting { dest });
            }
            let dest = allocator.reserve(dest, key, overwrite);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &png_bytes)?;
            Ok(DecodeAttempt::Exported {
                dest,
                output_bytes: Some(png_bytes.len() as u64),
            })
        })()
        .unwrap_or(DecodeAttempt::NotApplicable),
        _ => DecodeAttempt::NotApplicable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_address() -> ObjectAddress {
        ObjectAddress::binary_bundle_member(
            unity_asset::SourceLocator::path("game.ab").unwrap(),
            unity_asset::SourceMemberId::new("cab/main").unwrap(),
            1,
        )
        .unwrap()
    }

    fn manifest_entry(asset_path: &str, address: ObjectAddress) -> ExportManifestEntry {
        ExportManifestEntry {
            order: 0,
            asset_path: asset_path.into(),
            address,
            type_id: None,
            class_name: None,
            status: ExportStatus::Failed,
            output_path: None,
            output_bytes: None,
            error: Some("failed".into()),
        }
    }

    #[test]
    fn manifest_entry_rejects_malformed_object_address() {
        let value = serde_json::json!({
            "order": 0,
            "asset_path": "assets/example.bin",
            "address": "bok2|bundle|0|1|7|game.ab|0|",
            "type_id": null,
            "class_name": null,
            "status": "failed",
            "output_path": null,
            "output_bytes": null,
            "error": "failed"
        });

        assert!(serde_json::from_value::<ExportManifestEntry>(value).is_err());
    }

    #[test]
    fn resume_and_retry_reject_duplicate_asset_path_address_pairs() {
        let address = object_address();
        let duplicates = vec![
            manifest_entry("assets/a.bin", address.clone()),
            manifest_entry("assets/a.bin", address.clone()),
        ];
        assert!(ensure_unique_manifest_jobs(&duplicates, "--resume").is_err());
        assert!(ensure_unique_manifest_jobs(&duplicates, "--retry-failed-from").is_err());

        let aliases = vec![
            manifest_entry("assets/a.bin", address.clone()),
            manifest_entry("assets/b.bin", address),
        ];
        assert!(ensure_unique_manifest_jobs(&aliases, "--resume").is_ok());
    }

    #[test]
    fn fail_fast_outcome_keeps_the_typed_retry_address() {
        let address = object_address();
        let job = ExportJob {
            order: 3,
            asset_path: "assets/a.bin".into(),
            key: BinaryObjectKey {
                source: BinarySource::path("game.ab"),
                source_kind: unity_asset::environment::BinarySourceKind::AssetBundle,
                asset_index: Some(9),
                path_id: 1,
            },
            address: address.clone(),
        };

        let outcome = failed_export_outcome(&Environment::new(), &job, "write failed".into());
        assert!(matches!(outcome.entry.status, ExportStatus::Failed));
        assert_eq!(outcome.entry.address, address);
        let json = serde_json::to_value(outcome.entry).unwrap();
        assert!(json.get("address").is_some());
        assert!(json.get("key").is_none());
        assert!(json.get("asset_index").is_none());
        assert!(json.get("path_id").is_none());
        assert!(json.get("source_kind").is_none());
    }

    #[test]
    fn oversized_manifest_is_rejected_before_json_decode() {
        let oversized = std::io::Cursor::new(vec![b' '; 33]);
        let error = deserialize_export_manifest(oversized, 32).unwrap_err();
        assert!(error.to_string().contains("bytes"));
    }
}
