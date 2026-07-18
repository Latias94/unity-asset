use crate::shared::{
    AppContext, binary_object_key_for_address, build_environment, load_environment_input,
    object_address_for_key, resolve_loaded_source,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use unity_asset::environment::{BinaryObjectKey, BinarySource, BinarySourceKind, Environment};
use unity_asset::{AssetLoadBudget, AssetLoadLimits, ObjectAddress};
use unity_asset_binary::asset::SerializedFile;

#[cfg(feature = "decode")]
use unity_asset::UnityValue;
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
    unity_asset::get_class_name_str(class_id)
        .unwrap_or("Class_Unknown")
        .to_string()
}

fn source_rel_for_output(input: &Path, source: &BinarySource) -> String {
    let BinarySource::Path(p) = source else {
        return source.to_string();
    };

    if input.is_file() {
        return p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("serialized")
            .to_string();
    }

    if let Ok(rel) = p.strip_prefix(input) {
        if let Some(s) = rel.to_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }

    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("serialized")
        .to_string()
}

#[derive(Debug, Clone)]
struct ExportJob {
    order: usize,
    label: String,
    key: BinaryObjectKey,
    address: ObjectAddress,
    dest_base: PathBuf,
    decode: bool,
    overwrite: bool,
    effective_skip_existing: bool,
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
    address: ObjectAddress,
    class_id: Option<i32>,
    class_name: Option<String>,
    name: Option<String>,
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
    decode: bool,
    overwrite: bool,
    skip_existing: bool,
    jobs: usize,
    strict: bool,
    show_warnings: bool,
    limit: Option<usize>,
    class_ids: Vec<i32>,
    class_name: String,
    name: String,
    planned: usize,
    exported: usize,
    skipped_existing: usize,
    resumed: usize,
    failed: usize,
    entries: Vec<ExportManifestEntry>,
}

const EXPORT_MANIFEST_SCHEMA: u32 = 2;
const MAX_EXPORT_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn write_export_manifest(path: &Path, manifest: &ExportManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let tmp = path.with_extension("tmp");
    let file = std::fs::File::create(&tmp)?;
    serde_json::to_writer_pretty(&file, manifest)?;
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

fn ensure_unique_manifest_addresses(entries: &[ExportManifestEntry], usage: &str) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        if !seen.insert(entry.address.clone()) {
            anyhow::bail!(
                "Duplicate object address in {usage} manifest: {}",
                entry.address.to_compact_string()?
            );
        }
    }
    Ok(())
}

fn ensure_exports_succeeded(failed: usize) -> Result<()> {
    if failed > 0 {
        anyhow::bail!(
            "{failed} objects failed (use --manifest to inspect, or re-run with --retry-failed-from)"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    input: PathBuf,
    output: PathBuf,
    source: Option<PathBuf>,
    class_id: Vec<i32>,
    class_name: String,
    name: String,
    limit: Option<usize>,
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
    #[cfg(not(feature = "decode"))]
    if decode {
        anyhow::bail!(
            "--decode requires compiling `unity-asset-cli` with feature `decode` (build with default features, or `--features decode`)."
        );
    }

    let mut object_budget = AssetLoadBudget::default();
    let mut env = build_environment(
        ctx.strict,
        ctx.show_warnings,
        ctx.typetree_registries(),
        &mut object_budget,
    )?;
    load_environment_input(&mut env, &input, &mut object_budget)?;

    let mut resume_map: std::collections::HashMap<ObjectAddress, ExportManifestEntry> =
        std::collections::HashMap::new();
    if let Some(path) = resume.as_ref() {
        let prev = read_export_manifest(path)?;
        if prev.schema != EXPORT_MANIFEST_SCHEMA {
            anyhow::bail!("Unsupported --resume manifest schema: {}", prev.schema);
        }
        ensure_unique_manifest_addresses(&prev.entries, "--resume")?;
        for e in prev.entries {
            let address = e.address.clone();
            resume_map.insert(address, e);
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
        ensure_unique_manifest_addresses(&prev.entries, "--retry-failed-from")?;

        let mut jobs: Vec<ExportJob> = Vec::new();
        let mut order = 0usize;
        for e in prev.entries {
            if !matches!(e.status, ExportStatus::Failed) {
                continue;
            }
            let address = e.address.clone();
            let key = binary_object_key_for_address(&env, &input, &address)?;
            if key.source_kind != BinarySourceKind::SerializedFile {
                anyhow::bail!(
                    "--retry-failed-from entry {} is not a SerializedFile address",
                    address.to_compact_string()?
                );
            }

            let rel = source_rel_for_output(&input, &key.source);

            let src_dir = sanitize_asset_path(&rel);
            let class = e
                .class_name
                .clone()
                .unwrap_or_else(|| "Class_Unknown".to_string());
            let base_name = e
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!("{}_{}", s, key.path_id))
                .unwrap_or_else(|| format!("{}", key.path_id));

            let mut dest_base = output.join(&src_dir).join(sanitize_asset_path(&class));
            dest_base.push(sanitize_asset_path(&base_name));

            let label = format!("{}/{}#{}", rel, class, key.path_id);
            jobs.push(ExportJob {
                order,
                label,
                key,
                address,
                dest_base,
                decode,
                overwrite,
                effective_skip_existing: skip_existing,
            });
            order += 1;
        }
        retry_failed_jobs = Some(jobs);
    }

    let mut sources: Vec<BinarySource> = if let Some(source) = source.as_ref() {
        let resolved = resolve_loaded_source(
            &env,
            BinarySourceKind::SerializedFile,
            &BinarySource::path(source),
        )?;
        vec![resolved]
    } else {
        env.binary_assets().keys().cloned().collect()
    };
    sources.sort();

    if sources.is_empty() {
        println!("⚠ No SerializedFiles found in {:?}", input);
        return Ok(());
    }

    let class_name_lc = class_name.to_ascii_lowercase();
    let name_lc = name.to_ascii_lowercase();
    let has_name_filter = !name_lc.is_empty();

    let mut export_jobs: Vec<ExportJob> = Vec::new();
    let mut pre_entries: Vec<ExportManifestEntry> = Vec::new();
    let mut resumed = 0usize;
    let mut order = 0usize;

    if let Some(jobs) = retry_failed_jobs.take() {
        export_jobs = jobs;
    } else {
        let effective_skip_existing = skip_existing || resume.is_some();
        for src in &sources {
            let Some(file) = env.binary_assets().get(src) else {
                continue;
            };
            let src_rel = source_rel_for_output(&input, src);
            let src_dir = sanitize_asset_path(&src_rel);

            for handle in file.object_handles() {
                if let Some(max) = limit {
                    if export_jobs.len() + pre_entries.len() >= max {
                        break;
                    }
                }

                let cid = handle.class_id();
                if !class_id.is_empty() && !class_id.contains(&cid) {
                    continue;
                }

                let class = best_effort_class_name(file, cid);
                if !class_name_lc.is_empty() && !class.to_ascii_lowercase().contains(&class_name_lc)
                {
                    continue;
                }

                let peek_name = match handle.peek_name(&mut object_budget) {
                    Ok(name) => name,
                    Err(error) if error.is_resource_error() => return Err(error.into()),
                    Err(_) => None,
                };
                if has_name_filter
                    && !peek_name
                        .as_deref()
                        .map(|s| s.to_ascii_lowercase().contains(&name_lc))
                        .unwrap_or(false)
                {
                    continue;
                }

                let base_name = if let Some(name) = peek_name.as_deref() {
                    if name.is_empty() {
                        format!("{}", handle.path_id())
                    } else {
                        format!("{}_{}", name, handle.path_id())
                    }
                } else {
                    format!("{}", handle.path_id())
                };

                let mut dest_base = output.join(&src_dir).join(sanitize_asset_path(&class));
                dest_base.push(sanitize_asset_path(&base_name));

                let key = BinaryObjectKey {
                    source: src.clone(),
                    source_kind: BinarySourceKind::SerializedFile,
                    asset_index: None,
                    path_id: handle.path_id(),
                };
                let address = object_address_for_key(&env, &input, &key)?;

                if effective_skip_existing && !overwrite {
                    if let Some(prev) = resume_map.get(&address) {
                        if let Some(p) = prev.output_path.as_ref() {
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
                                pre_entries.push(ExportManifestEntry {
                                    order,
                                    address,
                                    class_id: Some(cid),
                                    class_name: Some(class.clone()),
                                    name: peek_name.clone(),
                                    status: ExportStatus::Resumed,
                                    output_path: Some(prev_path.to_string_lossy().to_string()),
                                    output_bytes: prev.output_bytes,
                                    error: None,
                                });
                                order += 1;
                                continue;
                            }
                        }
                    }
                }

                let label = format!("{}/{}#{}", src_rel, class, handle.path_id());
                export_jobs.push(ExportJob {
                    order,
                    label,
                    key,
                    address,
                    dest_base,
                    decode,
                    overwrite,
                    effective_skip_existing,
                });
                order += 1;
            }
        }
    }

    if export_jobs.is_empty() && pre_entries.is_empty() {
        if let Some(path) = manifest.as_ref() {
            let out = ExportManifest {
                schema: EXPORT_MANIFEST_SCHEMA,
                created_unix_ms: now_unix_ms(),
                input: input.to_string_lossy().to_string(),
                output: output.to_string_lossy().to_string(),
                decode,
                overwrite,
                skip_existing,
                jobs,
                strict: ctx.strict,
                show_warnings: ctx.show_warnings,
                limit,
                class_ids: class_id,
                class_name,
                name,
                planned: 0,
                exported: 0,
                skipped_existing: 0,
                resumed,
                failed: 0,
                entries: Vec::new(),
            };
            write_export_manifest(path.as_path(), &out)?;
        }
        println!("⚠ No matching objects found");
        return Ok(());
    }

    if dry_run {
        for j in export_jobs.iter().take(100) {
            println!("• would export {} -> {:?}", j.label, j.dest_base);
        }
        if export_jobs.len() > 100 {
            println!("... ({} more)", export_jobs.len() - 100);
        }

        if let Some(path) = manifest.as_ref() {
            let mut entries: Vec<ExportManifestEntry> =
                Vec::with_capacity(pre_entries.len() + export_jobs.len());
            entries.extend(pre_entries.clone());
            for j in export_jobs.iter() {
                entries.push(ExportManifestEntry {
                    order: j.order,
                    address: j.address.clone(),
                    class_id: None,
                    class_name: None,
                    name: None,
                    status: ExportStatus::Planned,
                    output_path: None,
                    output_bytes: None,
                    error: None,
                });
            }
            let out = ExportManifest {
                schema: EXPORT_MANIFEST_SCHEMA,
                created_unix_ms: now_unix_ms(),
                input: input.to_string_lossy().to_string(),
                output: output.to_string_lossy().to_string(),
                decode,
                overwrite,
                skip_existing,
                jobs,
                strict: ctx.strict,
                show_warnings: ctx.show_warnings,
                limit,
                class_ids: class_id,
                class_name,
                name,
                planned: entries.len(),
                exported: 0,
                skipped_existing: 0,
                resumed,
                failed: 0,
                entries,
            };
            write_export_manifest(path.as_path(), &out)?;
        }
        return Ok(());
    }

    std::fs::create_dir_all(&output)?;

    let threads = if jobs == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        jobs.max(1)
    };

    let env = Arc::new(env);
    let object_budget = Arc::new(std::sync::Mutex::new(object_budget));
    let exported = Arc::new(AtomicUsize::new(0));
    let skipped_existing_count = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let cancelled = Arc::new(AtomicBool::new(false));

    let manifest_entries: Arc<std::sync::Mutex<Vec<ExportManifestEntry>>> =
        Arc::new(std::sync::Mutex::new(pre_entries));

    let job_queue = Arc::new(std::sync::Mutex::new(export_jobs));
    let mut handles = Vec::new();

    for _ in 0..threads {
        let env = Arc::clone(&env);
        let exported = Arc::clone(&exported);
        let skipped_existing_count = Arc::clone(&skipped_existing_count);
        let failed = Arc::clone(&failed);
        let cancelled = Arc::clone(&cancelled);
        let job_queue = Arc::clone(&job_queue);
        let manifest_entries = Arc::clone(&manifest_entries);
        let object_budget = Arc::clone(&object_budget);
        handles.push(thread::spawn(move || {
            loop {
                if cancelled.load(Ordering::Relaxed) {
                    break;
                }
                let job = {
                    let mut q = job_queue.lock().unwrap();
                    q.pop()
                };
                let Some(job) = job else {
                    break;
                };

                match export_one_inner(&env, &job, &object_budget) {
                    Ok((dest, true, status, bytes, class_id, class_name, obj_name)) => {
                        println!("✓ {} -> {:?}", job.label, dest);
                        exported.fetch_add(1, Ordering::Relaxed);
                        let mut guard = manifest_entries.lock().unwrap();
                        guard.push(ExportManifestEntry {
                            order: job.order,
                            address: job.address.clone(),
                            class_id: Some(class_id),
                            class_name: Some(class_name),
                            name: obj_name,
                            status,
                            output_path: Some(dest.to_string_lossy().to_string()),
                            output_bytes: bytes,
                            error: None,
                        });
                    }
                    Ok((dest, false, status, bytes, class_id, class_name, obj_name)) => {
                        println!("↷ {} -> {:?} (skipped existing)", job.label, dest);
                        skipped_existing_count.fetch_add(1, Ordering::Relaxed);
                        let mut guard = manifest_entries.lock().unwrap();
                        guard.push(ExportManifestEntry {
                            order: job.order,
                            address: job.address.clone(),
                            class_id: Some(class_id),
                            class_name: Some(class_name),
                            name: obj_name,
                            status,
                            output_path: Some(dest.to_string_lossy().to_string()),
                            output_bytes: bytes,
                            error: None,
                        });
                    }
                    Err(e) => {
                        eprintln!("✗ {}: {}", job.label, e);
                        failed.fetch_add(1, Ordering::Relaxed);
                        let mut guard = manifest_entries.lock().unwrap();
                        guard.push(ExportManifestEntry {
                            order: job.order,
                            address: job.address.clone(),
                            class_id: None,
                            class_name: None,
                            name: None,
                            status: ExportStatus::Failed,
                            output_path: None,
                            output_bytes: None,
                            error: Some(e.to_string()),
                        });
                        if !continue_on_error {
                            cancelled.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        }));
    }

    for h in handles {
        let _ = h.join();
    }

    let failed_count = failed.load(Ordering::Relaxed);
    println!(
        "Exported {} objects, skipped existing {}, failed {} [jobs={}]",
        exported.load(Ordering::Relaxed),
        skipped_existing_count.load(Ordering::Relaxed),
        failed_count,
        threads
    );

    if let Some(path) = manifest.as_ref() {
        let mut entries = manifest_entries.lock().unwrap().clone();
        entries.sort_by_key(|e| e.order);
        let out = ExportManifest {
            schema: EXPORT_MANIFEST_SCHEMA,
            created_unix_ms: now_unix_ms(),
            input: input.to_string_lossy().to_string(),
            output: output.to_string_lossy().to_string(),
            decode,
            overwrite,
            skip_existing,
            jobs,
            strict: ctx.strict,
            show_warnings: ctx.show_warnings,
            limit,
            class_ids: class_id,
            class_name,
            name,
            planned: entries.len(),
            exported: exported.load(Ordering::Relaxed),
            skipped_existing: skipped_existing_count.load(Ordering::Relaxed),
            resumed,
            failed: failed_count,
            entries,
        };
        write_export_manifest(path.as_path(), &out)?;
    }

    ensure_exports_succeeded(failed_count)
}

type ExportOneInnerResult = (
    PathBuf,
    bool,
    ExportStatus,
    Option<u64>,
    i32,
    String,
    Option<String>,
);

fn export_one_inner(
    env: &Environment,
    job: &ExportJob,
    budget: &std::sync::Mutex<AssetLoadBudget>,
) -> Result<ExportOneInnerResult> {
    let obj = {
        let mut budget = budget
            .lock()
            .map_err(|_| anyhow::anyhow!("asset load budget lock poisoned"))?;
        env.read_binary_object_key(&job.key, &mut budget)?
    };
    let class_id = obj.info.class_id();
    let class_name = best_effort_class_name(
        env.binary_assets().get(&job.key.source).ok_or_else(|| {
            anyhow::anyhow!("SerializedFile source not loaded: {}", job.key.source)
        })?,
        class_id,
    );
    let obj_name = obj.name();

    if job.decode {
        #[cfg(feature = "decode")]
        if let Some((dest, exported, bytes)) =
            try_decode_export_best_effort(env, job, &obj, budget)?
        {
            let status = if exported {
                ExportStatus::ExportedDecoded
            } else {
                ExportStatus::SkippedExisting
            };
            return Ok((
                dest, exported, status, bytes, class_id, class_name, obj_name,
            ));
        }
    }

    let mut dest = job.dest_base.clone();
    dest.set_extension("bin");

    if job.effective_skip_existing && dest.exists() && !job.overwrite {
        let bytes = std::fs::metadata(&dest).map(|m| m.len()).ok();
        return Ok((
            dest,
            false,
            ExportStatus::SkippedExisting,
            bytes,
            class_id,
            class_name,
            obj_name,
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let bytes = obj.raw_data();
    std::fs::write(&dest, bytes)?;
    Ok((
        dest,
        true,
        ExportStatus::ExportedRaw,
        Some(bytes.len() as u64),
        class_id,
        class_name,
        obj_name,
    ))
}

#[cfg(feature = "decode")]
fn text_asset_bytes(obj: &UnityObject) -> Vec<u8> {
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

#[cfg(feature = "decode")]
fn try_decode_export_best_effort(
    env: &Environment,
    job: &ExportJob,
    obj: &UnityObject,
    budget: &std::sync::Mutex<AssetLoadBudget>,
) -> Result<Option<(PathBuf, bool, Option<u64>)>> {
    let unity_version = env
        .binary_assets()
        .get(&job.key.source)
        .map(|f| UnityVersion::parse_version(&f.unity_version).unwrap_or_default())
        .unwrap_or_default();

    let class_id = obj.info.class_id();

    match class_id {
        class_ids::AUDIO_CLIP => {
            let converter = AudioClipConverter::new(unity_version.clone());
            let clip = converter.from_unity_object(obj)?;

            let mut dest = job.dest_base.clone();
            match converter.get_audio_data(&clip) {
                Ok(audio_bytes) if !audio_bytes.is_empty() => {
                    dest.set_extension(clip.compression_format().extension());
                    if job.effective_skip_existing && dest.exists() && !job.overwrite {
                        let existing_len = std::fs::metadata(&dest).map(|m| m.len()).ok();
                        return Ok(Some((dest, false, existing_len)));
                    }
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, &audio_bytes)?;
                    return Ok(Some((dest, true, Some(audio_bytes.len() as u64))));
                }
                _ => {
                    if clip.is_streamed() {
                        if let Ok(bytes) = env.read_stream_data_source(
                            &job.key.source,
                            job.key.source_kind,
                            &clip.stream_info.path,
                            clip.stream_info.offset,
                            clip.stream_info.size,
                        ) {
                            if !bytes.is_empty() {
                                dest.set_extension(clip.compression_format().extension());
                                if job.effective_skip_existing && dest.exists() && !job.overwrite {
                                    let existing_len =
                                        std::fs::metadata(&dest).map(|m| m.len()).ok();
                                    return Ok(Some((dest, false, existing_len)));
                                }
                                if let Some(parent) = dest.parent() {
                                    std::fs::create_dir_all(parent)?;
                                }
                                std::fs::write(&dest, &bytes)?;
                                return Ok(Some((dest, true, Some(bytes.len() as u64))));
                            }
                        }
                    }

                    dest.set_extension("wav");
                    if job.effective_skip_existing && dest.exists() && !job.overwrite {
                        let existing_len = std::fs::metadata(&dest).map(|m| m.len()).ok();
                        return Ok(Some((dest, false, existing_len)));
                    }
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let audio_processor = AudioProcessor::new(unity_version);
                    audio_processor.process_and_export(obj, &dest)?;
                    let written_len = std::fs::metadata(&dest).map(|m| m.len()).ok();
                    return Ok(Some((dest, true, written_len)));
                }
            }
        }
        class_ids::TEXTURE_2D => {
            let mut dest = job.dest_base.clone();
            dest.set_extension("png");
            if job.effective_skip_existing && dest.exists() && !job.overwrite {
                let existing_len = std::fs::metadata(&dest).map(|m| m.len()).ok();
                return Ok(Some((dest, false, existing_len)));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let texture_processor = TextureProcessor::new(unity_version);
            let mut texture = texture_processor.convert_object(obj)?;
            if texture.image_data.is_empty() && texture.is_streamed() {
                if let Ok(bytes) = env.read_stream_data_source(
                    &job.key.source,
                    job.key.source_kind,
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
            let written_len = std::fs::metadata(&dest).map(|m| m.len()).ok();
            return Ok(Some((dest, true, written_len)));
        }
        class_ids::TEXT_ASSET => {
            let bytes = text_asset_bytes(obj);
            if bytes.is_empty() {
                return Ok(None);
            }

            let mut dest = job.dest_base.clone();
            dest.set_extension(if std::str::from_utf8(&bytes).is_ok() {
                "txt"
            } else {
                "bin"
            });
            if job.effective_skip_existing && dest.exists() && !job.overwrite {
                let existing_len = std::fs::metadata(&dest).map(|m| m.len()).ok();
                return Ok(Some((dest, false, existing_len)));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &bytes)?;
            return Ok(Some((dest, true, Some(bytes.len() as u64))));
        }
        class_ids::SPRITE => {
            let Some(obj_ref) =
                env.find_binary_object_in_source_id(&job.key.source, job.key.path_id)
            else {
                return Ok(None);
            };

            let sprite_processor = SpriteProcessor::new(unity_version.clone());
            let sprite = sprite_processor.parse_sprite(obj)?.sprite;

            let (file_id, texture_path_id) =
                if let Some((file_id, path_id)) = sprite_texture_pptr(obj) {
                    (file_id, path_id)
                } else if sprite.render_data.texture_path_id != 0 {
                    (0, sprite.render_data.texture_path_id)
                } else {
                    return Ok(None);
                };

            let texture_obj = {
                let mut budget = budget
                    .lock()
                    .map_err(|_| anyhow::anyhow!("asset load budget lock poisoned"))?;
                env.read_binary_pptr(&obj_ref, file_id, texture_path_id, &mut budget)?
            };

            let texture_processor = TextureProcessor::new(unity_version);
            let mut texture = texture_processor.convert_object(&texture_obj)?;
            if texture.image_data.is_empty() && texture.is_streamed() {
                if let Ok(bytes) = env.read_stream_data_source(
                    &job.key.source,
                    job.key.source_kind,
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
            let mut dest = job.dest_base.clone();
            dest.set_extension("png");
            if job.effective_skip_existing && dest.exists() && !job.overwrite {
                let existing_len = std::fs::metadata(&dest).map(|m| m.len()).ok();
                return Ok(Some((dest, false, existing_len)));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &png_bytes)?;
            return Ok(Some((dest, true, Some(png_bytes.len() as u64))));
        }
        _ => {}
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_entry(address: ObjectAddress) -> ExportManifestEntry {
        ExportManifestEntry {
            order: 0,
            address,
            class_id: None,
            class_name: None,
            name: None,
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
            "address": "bok2|serialized|-|1|8|game.ab|0|",
            "class_id": null,
            "class_name": null,
            "name": null,
            "status": "failed",
            "output_path": null,
            "output_bytes": null,
            "error": "failed"
        });

        assert!(serde_json::from_value::<ExportManifestEntry>(value).is_err());
    }

    #[test]
    fn resume_and_retry_reject_duplicate_addresses() {
        let address = ObjectAddress::binary_direct(
            unity_asset::SourceLocator::path("game.assets").unwrap(),
            1,
        )
        .unwrap();
        let entries = vec![manifest_entry(address.clone()), manifest_entry(address)];

        assert!(ensure_unique_manifest_addresses(&entries, "--resume").is_err());
        assert!(ensure_unique_manifest_addresses(&entries, "--retry-failed-from").is_err());

        let json = serde_json::to_value(&entries[0]).unwrap();
        assert!(json.get("address").is_some());
        assert!(json.get("key").is_none());
        assert!(json.get("source").is_none());
        assert!(json.get("path_id").is_none());
    }

    #[test]
    fn oversized_manifest_is_rejected_before_json_decode() {
        let oversized = std::io::Cursor::new(vec![b' '; 33]);
        let error = deserialize_export_manifest(oversized, 32).unwrap_err();
        assert!(error.to_string().contains("bytes"));
    }

    #[test]
    fn failed_exports_return_an_error() {
        assert!(ensure_exports_succeeded(0).is_ok());
        assert!(ensure_exports_succeeded(1).is_err());
    }
}
