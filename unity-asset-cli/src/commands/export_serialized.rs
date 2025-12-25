use crate::shared::{
    AppContext, build_environment, load_environment_input, resolve_loaded_source,
};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use unity_asset::environment::{BinaryObjectKey, BinarySource, BinarySourceKind, Environment};
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
    label: String,
    key: BinaryObjectKey,
    dest_base: PathBuf,
    decode: bool,
    overwrite: bool,
    skip_existing: bool,
}

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
    jobs: usize,
    ctx: &AppContext,
) -> Result<()> {
    #[cfg(not(feature = "decode"))]
    if decode {
        anyhow::bail!(
            "--decode requires compiling `unity-asset-cli` with feature `decode` (build with default features, or `--features decode`)."
        );
    }

    let mut env = build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;
    load_environment_input(&mut env, &input)?;

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

    for src in &sources {
        let Some(file) = env.binary_assets().get(src) else {
            continue;
        };
        let src_rel = source_rel_for_output(&input, src);
        let src_dir = sanitize_asset_path(&src_rel);

        for handle in file.object_handles() {
            if let Some(max) = limit {
                if export_jobs.len() >= max {
                    break;
                }
            }

            let cid = handle.class_id();
            if !class_id.is_empty() && !class_id.contains(&cid) {
                continue;
            }

            let class = best_effort_class_name(file, cid);
            if !class_name_lc.is_empty() && !class.to_ascii_lowercase().contains(&class_name_lc) {
                continue;
            }

            let peek_name = handle.peek_name().ok().flatten();
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

            let label = format!("{}/{}#{}", src_rel, class, handle.path_id());
            export_jobs.push(ExportJob {
                label,
                key,
                dest_base,
                decode,
                overwrite,
                skip_existing,
            });
        }
    }

    if export_jobs.is_empty() {
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
    let exported = Arc::new(AtomicUsize::new(0));
    let skipped_existing_count = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));

    let job_queue = Arc::new(std::sync::Mutex::new(export_jobs));
    let mut handles = Vec::new();

    for _ in 0..threads {
        let env = Arc::clone(&env);
        let exported = Arc::clone(&exported);
        let skipped_existing_count = Arc::clone(&skipped_existing_count);
        let failed = Arc::clone(&failed);
        let job_queue = Arc::clone(&job_queue);
        handles.push(thread::spawn(move || {
            loop {
                let job = {
                    let mut q = job_queue.lock().unwrap();
                    q.pop()
                };
                let Some(job) = job else {
                    break;
                };

                match export_one_inner(&env, &job) {
                    Ok((dest, true)) => {
                        println!("✓ {} -> {:?}", job.label, dest);
                        exported.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok((dest, false)) => {
                        println!("↷ {} -> {:?} (skipped existing)", job.label, dest);
                        skipped_existing_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("✗ {}: {}", job.label, e);
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    for h in handles {
        let _ = h.join();
    }

    println!(
        "Exported {} objects, skipped existing {}, failed {} [jobs={}]",
        exported.load(Ordering::Relaxed),
        skipped_existing_count.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
        threads
    );

    Ok(())
}

fn export_one_inner(env: &Environment, job: &ExportJob) -> Result<(PathBuf, bool)> {
    if job.decode {
        #[cfg(feature = "decode")]
        if let Some((dest, exported)) = try_decode_export_best_effort(env, job)? {
            return Ok((dest, exported));
        }
    }

    let mut dest = job.dest_base.clone();
    dest.set_extension("bin");

    if job.skip_existing && dest.exists() && !job.overwrite {
        return Ok((dest, false));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let obj = env.read_binary_object_key(&job.key)?;
    let bytes = obj.raw_data();
    std::fs::write(&dest, bytes)?;
    Ok((dest, true))
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
fn try_decode_export_best_effort(env: &Environment, job: &ExportJob) -> Result<Option<(PathBuf, bool)>> {
    let obj = env.read_binary_object_key(&job.key)?;
    let unity_version = env
        .binary_assets()
        .get(&job.key.source)
        .map(|f| UnityVersion::parse_version(&f.unity_version).unwrap_or_default())
        .unwrap_or_default();

    let class_id = obj.info.type_id;

    match class_id {
        class_ids::AUDIO_CLIP => {
            let converter = AudioClipConverter::new(unity_version.clone());
            let clip = converter.from_unity_object(&obj)?;

            let mut dest = job.dest_base.clone();
            match converter.get_audio_data(&clip) {
                Ok(audio_bytes) if !audio_bytes.is_empty() => {
                    dest.set_extension(clip.compression_format().extension());
                    if job.skip_existing && dest.exists() && !job.overwrite {
                        return Ok(Some((dest, false)));
                    }
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, &audio_bytes)?;
                    return Ok(Some((dest, true)));
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
                                if job.skip_existing && dest.exists() && !job.overwrite {
                                    return Ok(Some((dest, false)));
                                }
                                if let Some(parent) = dest.parent() {
                                    std::fs::create_dir_all(parent)?;
                                }
                                std::fs::write(&dest, &bytes)?;
                                return Ok(Some((dest, true)));
                            }
                        }
                    }

                    dest.set_extension("wav");
                    if job.skip_existing && dest.exists() && !job.overwrite {
                        return Ok(Some((dest, false)));
                    }
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let audio_processor = AudioProcessor::new(unity_version);
                    audio_processor.process_and_export(&obj, &dest)?;
                    return Ok(Some((dest, true)));
                }
            }
        }
        class_ids::TEXTURE_2D => {
            let mut dest = job.dest_base.clone();
            dest.set_extension("png");
            if job.skip_existing && dest.exists() && !job.overwrite {
                return Ok(Some((dest, false)));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let texture_processor = TextureProcessor::new(unity_version);
            let mut texture = texture_processor.convert_object(&obj)?;
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
            return Ok(Some((dest, true)));
        }
        class_ids::TEXT_ASSET => {
            let bytes = text_asset_bytes(&obj);
            if bytes.is_empty() {
                return Ok(None);
            }

            let mut dest = job.dest_base.clone();
            dest.set_extension(if std::str::from_utf8(&bytes).is_ok() { "txt" } else { "bin" });
            if job.skip_existing && dest.exists() && !job.overwrite {
                return Ok(Some((dest, false)));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &bytes)?;
            return Ok(Some((dest, true)));
        }
        class_ids::SPRITE => {
            let Some(obj_ref) = env.find_binary_object_in_source_id(&job.key.source, job.key.path_id) else {
                return Ok(None);
            };

            let sprite_processor = SpriteProcessor::new(unity_version.clone());
            let sprite = sprite_processor.parse_sprite(&obj)?.sprite;

            let (file_id, texture_path_id) = if let Some((file_id, path_id)) = sprite_texture_pptr(&obj) {
                (file_id, path_id)
            } else if sprite.render_data.texture_path_id != 0 {
                (0, sprite.render_data.texture_path_id)
            } else {
                return Ok(None);
            };

            let texture_obj = env.read_binary_pptr(&obj_ref, file_id, texture_path_id)?;

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
            if job.skip_existing && dest.exists() && !job.overwrite {
                return Ok(Some((dest, false)));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &png_bytes)?;
            return Ok(Some((dest, true)));
        }
        _ => {}
    }

    Ok(None)
}
