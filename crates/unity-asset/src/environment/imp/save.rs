use super::Environment;
use crate::Result;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use unity_asset_core::UnityAssetError;
use unity_asset_write::PackerOptions;
use unity_asset_write::bundle::{BundleEdits, BundleWriter};
use unity_asset_write::serialized_file::SerializedFileWriter;
use unity_asset_write::webfile::{WebFileEdits, WebFilePacker, WebFileWriter};

impl Environment {
    /// Save changed assets to `out_dir` (UnityPy-style).
    ///
    /// This writes sources that have pending edits recorded via `Environment::edit_binary_object_key`
    /// (or `Environment::edit_session()`).
    ///
    /// Current scope:
    /// - standalone `SerializedFile` save
    /// - UnityFS `AssetBundle` repack (replacing edited embedded SerializedFiles)
    /// - `WebFile` repack (replacing edited embedded entries)
    /// - `.resS`/`.resource` write support via `EnvironmentEditSession::write_to_cab`:
    ///   - embedded into bundles/webfiles
    ///   - written as sidecar files for standalone serialized files
    ///
    /// Not yet implemented:
    /// - modifying existing `.resS` payloads referenced by objects without updating their offsets
    pub fn save<P: AsRef<Path>>(&mut self, pack: PackerOptions, out_dir: P) -> Result<()> {
        let out_dir = out_dir.as_ref();
        fs::create_dir_all(out_dir)?;

        if !self.has_pending_writes() {
            return Ok(());
        }

        let mut state = self.take_write_state();
        let result = save_impl(self, &pack, out_dir, &mut state);
        match result {
            Ok(()) => {
                // UnityPy clears the changed flags after saving. We drop the pending write state.
                Ok(())
            }
            Err(e) => {
                // Preserve pending writes on failure.
                self.restore_write_state(state);
                Err(e)
            }
        }
    }
}

fn save_impl(
    env: &Environment,
    pack: &PackerOptions,
    out_dir: &Path,
    state: &mut super::edit::EnvironmentWriteState,
) -> Result<()> {
    let mut webfile_edits: HashMap<std::path::PathBuf, WebFileEdits> = HashMap::new();

    // 1) Save standalone SerializedFiles.
    for (source, file_state) in state.standalone.iter_mut() {
        if file_state.edits.is_empty() && file_state.cabs.is_empty() {
            continue;
        }

        let file = env.binary_assets.get(source).ok_or_else(|| {
            UnityAssetError::format(format!(
                "SerializedFile source not loaded: {}",
                source.describe()
            ))
        })?;

        let bytes = SerializedFileWriter::save(file, &file_state.edits)?;
        match source {
            super::BinarySource::Path(_) => {
                let out_name = output_name_for_source(source)?;
                fs::write(out_dir.join(out_name), bytes)?;

                // For standalone SerializedFiles, write `.resS`/`.resource` sidecars under
                // `out_dir/{asset_file_name}_data/{cab_name}` to avoid collisions with the file.
                if !file_state.cabs.is_empty() {
                    let cab_dir = out_dir.join(format!("{}_data", out_name.to_string_lossy()));
                    fs::create_dir_all(&cab_dir)?;
                    for cab in file_state.cabs.values() {
                        fs::write(cab_dir.join(&cab.name), cab.bytes())?;
                    }
                }
            }
            super::BinarySource::WebEntry {
                web_path,
                entry_name,
            } => {
                webfile_edits
                    .entry(web_path.clone())
                    .or_default()
                    .replace_file_bytes(entry_name.clone(), bytes);
            }
        }
    }

    // 2) Repack bundles that contain edited embedded SerializedFiles.
    for (bundle_source, bundle_state) in state.bundles.iter_mut() {
        if bundle_state.assets.is_empty() && bundle_state.cabs.is_empty() {
            continue;
        }

        let bundle = env.bundles.get(bundle_source).ok_or_else(|| {
            UnityAssetError::format(format!(
                "AssetBundle source not loaded: {}",
                bundle_source.describe()
            ))
        })?;

        let mut edits = BundleEdits::default();
        for (asset_index, asset_state) in bundle_state.assets.iter_mut() {
            if asset_state.edits.is_empty() {
                continue;
            }
            let asset = bundle.assets.get(*asset_index).ok_or_else(|| {
                UnityAssetError::format(format!(
                    "AssetBundle asset_index out of range: {} asset_index={}",
                    bundle_source.describe(),
                    asset_index
                ))
            })?;

            let node_name = bundle.asset_names.get(*asset_index).ok_or_else(|| {
                UnityAssetError::format(format!(
                    "AssetBundle asset name missing: {} asset_index={}",
                    bundle_source.describe(),
                    asset_index
                ))
            })?;

            let bytes = SerializedFileWriter::save(asset, &asset_state.edits)?;
            edits.replace_file_bytes(node_name.clone(), bytes);
        }

        for cab in bundle_state.cabs.values() {
            edits.add_file_bytes(cab.name.clone(), cab.bytes().to_vec(), cab.flags);
        }

        if edits.is_empty() {
            continue;
        }

        let bytes = BundleWriter::save(bundle, &edits, *pack)?;
        match bundle_source {
            super::BinarySource::Path(_) => {
                let out_name = output_name_for_source(bundle_source)?;
                fs::write(out_dir.join(out_name), bytes)?;
            }
            super::BinarySource::WebEntry {
                web_path,
                entry_name,
            } => {
                webfile_edits
                    .entry(web_path.clone())
                    .or_default()
                    .replace_file_bytes(entry_name.clone(), bytes);
            }
        }
    }

    // 3) Repack WebFiles that have pending writable cabs or edited entries.
    for (web_path, web_state) in state.webfiles.iter_mut() {
        for cab in web_state.cabs.values() {
            webfile_edits
                .entry(web_path.clone())
                .or_default()
                .replace_file_bytes(cab.name.clone(), cab.bytes().to_vec());
        }
    }

    // 3) Repack WebFiles that contain edited entries.
    for (web_path, edits) in webfile_edits.iter() {
        if edits.is_empty() {
            continue;
        }

        let web = resolve_webfile(env, web_path)?;
        let bytes = WebFileWriter::save(web, edits, WebFilePacker::None, None)?;

        let out_name = web_path.file_name().ok_or_else(|| {
            UnityAssetError::format(format!(
                "Invalid WebFile path: {}",
                web_path.to_string_lossy()
            ))
        })?;
        fs::write(out_dir.join(out_name), bytes)?;
    }

    // 4) Save edited YAML documents (prefab/scene/etc) to out_dir.
    for (path, doc) in state.yaml_documents.iter() {
        let out_path = output_path_for_yaml(env, out_dir, path)?;
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        doc.save_to(&out_path)?;
    }

    Ok(())
}

fn output_name_for_source(source: &super::BinarySource) -> Result<&OsStr> {
    match source {
        super::BinarySource::Path(p) => Ok(p.file_name().ok_or_else(|| {
            UnityAssetError::format(format!("Invalid source path: {}", p.to_string_lossy()))
        })?),
        super::BinarySource::WebEntry { entry_name, .. } => Ok(OsStr::new(entry_name)),
    }
}

fn output_path_for_yaml(
    env: &Environment,
    out_dir: &Path,
    path: &Path,
) -> Result<std::path::PathBuf> {
    if let Ok(rel) = path.strip_prefix(&env.base_path) {
        return Ok(out_dir.join(rel));
    }

    let name = path.file_name().ok_or_else(|| {
        UnityAssetError::format(format!("Invalid YAML path: {}", path.to_string_lossy()))
    })?;
    Ok(out_dir.join(name))
}

fn resolve_webfile<'a>(
    env: &'a Environment,
    web_path: &std::path::PathBuf,
) -> Result<&'a unity_asset_binary::webfile::WebFile> {
    if let Some(v) = env.webfiles.get(web_path) {
        return Ok(v);
    }

    let alt = super::path::canonicalize_if_exists(web_path);
    if let Some(v) = env.webfiles.get(&alt) {
        return Ok(v);
    }

    Err(UnityAssetError::format(format!(
        "WebFile not loaded: {}",
        web_path.to_string_lossy()
    )))
}
