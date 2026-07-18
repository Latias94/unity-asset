use super::path::canonicalize_source_if_possible;
use super::*;

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use unity_asset_binary::asset::FileIdentifier;
use unity_asset_core::{UnityAssetError, UnityClass};
use unity_asset_write::object::SerializedFileEditSession;
use unity_asset_write::resources::WritableCab;
use unity_asset_write::serialized_file::SerializedFileEdits;

#[derive(Debug, Default)]
pub struct EnvironmentWriteState {
    pub(crate) standalone: HashMap<BinarySource, SerializedFileWriteState>,
    pub(crate) bundles: HashMap<BinarySource, BundleWriteState>,
    pub(crate) webfiles: HashMap<std::path::PathBuf, WebFileWriteState>,
    pub(crate) yaml_documents: HashMap<std::path::PathBuf, YamlDocument>,
}

#[derive(Debug, Default)]
pub(crate) struct SerializedFileWriteState {
    pub(crate) edits: SerializedFileEdits,
    pub(crate) classes: HashMap<i64, UnityClass>,
    pub(crate) cabs: HashMap<String, WritableCab>,
}

#[derive(Debug, Default)]
pub(crate) struct BundleWriteState {
    // asset_index -> edits/classes for that embedded SerializedFile
    pub(crate) assets: HashMap<usize, SerializedFileWriteState>,
    pub(crate) cabs: HashMap<String, WritableCab>,
}

#[derive(Debug, Default)]
pub(crate) struct WebFileWriteState {
    pub(crate) cabs: HashMap<String, WritableCab>,
}

impl EnvironmentWriteState {
    pub fn is_empty(&self) -> bool {
        self.standalone.is_empty()
            && self.bundles.is_empty()
            && self.webfiles.is_empty()
            && self.yaml_documents.is_empty()
    }
}

/// A UnityPy-like edit session that records changes inside an `Environment`.
///
/// This is a convenience wrapper around `Environment` mutation APIs. Calling `save(...)` on the
/// environment will write only changed sources and then clear the pending edits.
pub struct EnvironmentEditSession<'a> {
    env: &'a mut Environment,
    budget: &'a mut AssetLoadBudget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedResourceWrite {
    pub path: String,
    pub offset: u64,
    pub size: u32,
}

impl<'a> EnvironmentEditSession<'a> {
    pub fn new(env: &'a mut Environment, budget: &'a mut AssetLoadBudget) -> Self {
        Self { env, budget }
    }

    pub fn env(&self) -> &Environment {
        self.env
    }

    pub fn env_mut(&mut self) -> &mut Environment {
        self.env
    }

    pub fn edit_binary_object_key(
        &mut self,
        key: &BinaryObjectKey,
        f: impl FnOnce(&mut UnityClass) -> Result<()>,
    ) -> Result<()> {
        self.env.edit_binary_object_key(key, self.budget, f)
    }

    /// Replace the entire parsed `UnityClass` of a binary object and record it as a pending write.
    ///
    /// This is the closest Rust equivalent to UnityPy's `Object.save()` workflow: parse an object,
    /// mutate it outside a closure, then persist it back into its source file.
    ///
    /// Notes:
    /// - The provided `class.class_id` must match the target object's class id.
    pub fn save_binary_object_class(
        &mut self,
        key: &BinaryObjectKey,
        class: UnityClass,
    ) -> Result<()> {
        let expected = expected_class_id_for_key(self.env, key)?;
        if class.class_id != expected {
            return Err(UnityAssetError::format(format!(
                "Class id mismatch for {} path_id={}: expected={}, got={}",
                key.source.describe(),
                key.path_id,
                expected,
                class.class_id
            )));
        }

        self.edit_binary_object_key(key, move |current| {
            *current = class;
            Ok(())
        })
    }

    /// Append `data` into a UnityPy-style writable cab (e.g. `CAB-UnityPy_Mod.resS`) and return the
    /// `(path, offset, size)` triple that can be written into streamed-resource fields.
    ///
    /// Notes:
    /// - For objects inside bundles, the cab is embedded into the bundle being saved.
    /// - For `SerializedFile` entries inside a WebFile container, the cab is embedded into that WebFile.
    /// - Standalone SerializedFiles are written as sidecar files under `out/{file}_data/{cab}`.
    pub fn write_to_cab(
        &mut self,
        key: &BinaryObjectKey,
        cab_name: Option<&str>,
        data: &[u8],
    ) -> Result<StreamedResourceWrite> {
        self.env.write_to_cab(key, cab_name, data)
    }

    /// Write bytes into a cab and update a streamed-resource field (e.g. `m_StreamData`) in-place.
    pub fn write_streamed_resource_to_field(
        &mut self,
        key: &BinaryObjectKey,
        field_name: &str,
        cab_name: Option<&str>,
        data: &[u8],
    ) -> Result<StreamedResourceWrite> {
        self.write_streamed_resource_transaction(key, cab_name, data, |class, write| {
            super::streamed_write::apply_streamed_resource_write(class, field_name, write)
        })
    }

    pub(super) fn write_streamed_resource_transaction(
        &mut self,
        key: &BinaryObjectKey,
        cab_name: Option<&str>,
        data: &[u8],
        apply: impl FnOnce(&mut UnityClass, &StreamedResourceWrite) -> Result<()>,
    ) -> Result<StreamedResourceWrite> {
        self.env
            .write_streamed_resource_transaction(key, cab_name, data, self.budget, apply)
    }

    pub fn save<P: AsRef<Path>>(
        &mut self,
        pack: unity_asset_write::PackerOptions,
        out_dir: P,
    ) -> Result<()> {
        self.env.save(pack, out_dir)
    }

    /// Set a value at a dot-separated field path (supports array indices like `m_Container[0].data`).
    ///
    /// This is a convenience wrapper around `edit_binary_object_key` + `pptr_path::set_value_at_path`.
    pub fn set_binary_value_at_path(
        &mut self,
        key: &BinaryObjectKey,
        field_path: &str,
        value: UnityValue,
    ) -> Result<()> {
        self.edit_binary_object_key(key, |class| {
            super::pptr_path::set_value_at_path(class, field_path, value)
        })
    }

    /// Read a value at a dot-separated field path (supports array indices like `m_Container[0].data`).
    ///
    /// This reads from pending edits when present, falling back to parsing the object from the loaded source.
    pub fn get_binary_value_at_path(
        &mut self,
        key: &BinaryObjectKey,
        field_path: &str,
    ) -> Result<Option<UnityValue>> {
        let class = self.read_binary_object_class_for_view(key)?;
        Ok(super::pptr_path::get_value_at_path(&class, field_path).cloned())
    }

    /// Resolve a `PPtr` stored at a dot-separated field path (e.g. `m_RD.texture`) to a globally-unique object key.
    ///
    /// Best-effort: may load missing dependency files on demand (UnityPy `Environment.find_file`-style).
    pub fn resolve_pptr_path_key(
        &mut self,
        context_key: &BinaryObjectKey,
        pptr_path: &str,
    ) -> Result<Option<BinaryObjectKey>> {
        self.env
            .resolve_pptr_path_key_best_effort(context_key, pptr_path, self.budget)
    }

    /// Set a `PPtr` stored at a dot-separated field path (e.g. `m_RD.texture`) to point at `target_key`.
    ///
    /// This best-effort helper also appends a new external entry (when needed) and returns the
    /// resulting `(file_id, path_id)` pair written into the object.
    pub fn set_pptr_path_to_key(
        &mut self,
        context_key: &BinaryObjectKey,
        pptr_path: &str,
        target_key: &BinaryObjectKey,
    ) -> Result<(i32, i64)> {
        self.env
            .set_pptr_path_to_key(context_key, pptr_path, target_key, self.budget)
    }

    /// Ensure the context serialized file has an external mapping for `target_key` and return the
    /// `fileID` to use in a `PPtr` field.
    pub fn file_id_for_target(
        &mut self,
        context_key: &BinaryObjectKey,
        target_key: &BinaryObjectKey,
    ) -> Result<i32> {
        self.env.file_id_for_target(context_key, target_key)
    }

    fn read_binary_object_class_for_view(&mut self, key: &BinaryObjectKey) -> Result<UnityClass> {
        match key.source_kind {
            BinarySourceKind::SerializedFile => {
                let (source_key, file) =
                    resolve_serialized_file_source(&self.env.binary_assets, &key.source)?;
                if let Some(state) = self.env.write_state.standalone.get(source_key)
                    && let Some(class) = state.classes.get(&key.path_id)
                {
                    return Ok(class.clone());
                }

                let handle = file.find_object_handle(key.path_id).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Object not found in SerializedFile {}: path_id={}",
                        key.source.describe(),
                        key.path_id
                    ))
                })?;
                let parsed = handle.read(self.budget).map_err(|e| {
                    UnityAssetError::with_source("Failed to parse binary object", e)
                })?;
                Ok(parsed.class)
            }
            BinarySourceKind::AssetBundle => {
                let asset_index = key.asset_index.ok_or_else(|| {
                    UnityAssetError::format(
                        "AssetBundle key requires an asset_index (which asset in the bundle?)"
                            .to_string(),
                    )
                })?;
                let (bundle_source_key, bundle) =
                    resolve_bundle_source(&self.env.bundles, &key.source)?;

                if let Some(bundle_state) = self.env.write_state.bundles.get(bundle_source_key)
                    && let Some(asset_state) = bundle_state.assets.get(&asset_index)
                    && let Some(class) = asset_state.classes.get(&key.path_id)
                {
                    return Ok(class.clone());
                }

                let file = bundle.assets.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset index out of range: {} asset_index={}",
                        key.source.describe(),
                        asset_index
                    ))
                })?;
                let handle = file.find_object_handle(key.path_id).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Object not found in AssetBundle {} asset_index={}: path_id={}",
                        key.source.describe(),
                        asset_index,
                        key.path_id
                    ))
                })?;
                let parsed = handle.read(self.budget).map_err(|e| {
                    UnityAssetError::with_source("Failed to parse binary object", e)
                })?;
                Ok(parsed.class)
            }
        }
    }
}

impl Environment {
    pub fn edit_session<'a>(
        &'a mut self,
        budget: &'a mut AssetLoadBudget,
    ) -> EnvironmentEditSession<'a> {
        EnvironmentEditSession::new(self, budget)
    }

    pub(crate) fn take_write_state(&mut self) -> EnvironmentWriteState {
        std::mem::take(&mut self.write_state)
    }

    pub(crate) fn restore_write_state(&mut self, state: EnvironmentWriteState) {
        self.write_state = state;
    }

    pub fn has_pending_writes(&self) -> bool {
        !self.write_state.is_empty()
    }

    pub fn edit_binary_object_key(
        &mut self,
        key: &BinaryObjectKey,
        budget: &mut AssetLoadBudget,
        f: impl FnOnce(&mut UnityClass) -> Result<()>,
    ) -> Result<()> {
        match key.source_kind {
            BinarySourceKind::SerializedFile => {
                let (source_key, file) =
                    resolve_serialized_file_source(&self.binary_assets, &key.source)?;
                let source_key = source_key.clone();
                let prepared = prepare_serialized_file_edit(
                    file,
                    self.write_state.standalone.get(&source_key),
                    key.path_id,
                    budget,
                    f,
                )?;
                let state = self.write_state.standalone.entry(source_key).or_default();
                apply_serialized_file_edit(state, key.path_id, prepared);
                Ok(())
            }
            BinarySourceKind::AssetBundle => {
                let asset_index = key.asset_index.ok_or_else(|| {
                    UnityAssetError::format("AssetBundle key requires an asset_index")
                })?;
                let (bundle_source_key, bundle) =
                    resolve_bundle_source(&self.bundles, &key.source)?;
                let bundle_source_key = bundle_source_key.clone();
                let asset = bundle.assets.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset index out of range: {} asset_index={}",
                        key.source.describe(),
                        asset_index
                    ))
                })?;

                let existing = self
                    .write_state
                    .bundles
                    .get(&bundle_source_key)
                    .and_then(|state| state.assets.get(&asset_index));
                let prepared =
                    prepare_serialized_file_edit(asset, existing, key.path_id, budget, f)?;
                let state = self
                    .write_state
                    .bundles
                    .entry(bundle_source_key)
                    .or_default()
                    .assets
                    .entry(asset_index)
                    .or_default();
                apply_serialized_file_edit(state, key.path_id, prepared);
                Ok(())
            }
        }
    }

    pub fn write_to_cab(
        &mut self,
        key: &BinaryObjectKey,
        cab_name: Option<&str>,
        data: &[u8],
    ) -> Result<StreamedResourceWrite> {
        let cab_name = cab_name.unwrap_or("CAB-UnityPy_Mod.resS");

        match key.source_kind {
            BinarySourceKind::AssetBundle => {
                let asset_index = key.asset_index.ok_or_else(|| {
                    UnityAssetError::format("AssetBundle key requires an asset_index")
                })?;
                let (bundle_source_key, bundle) =
                    resolve_bundle_source(&self.bundles, &key.source)?;
                let bundle_source_key = bundle_source_key.clone();

                let node_name = bundle.asset_names.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset name missing: {} asset_index={}",
                        key.source.describe(),
                        asset_index
                    ))
                })?;

                let cab_path = format!("archive:/{}/{}", node_name, cab_name);

                let flags = bundle
                    .nodes
                    .iter()
                    .find(|n| {
                        n.is_file() && (n.name.ends_with(".resS") || n.name.ends_with(".resource"))
                    })
                    .map(|n| n.flags)
                    .unwrap_or(0)
                    | 0x4;

                let bundle_state = self
                    .write_state
                    .bundles
                    .entry(bundle_source_key)
                    .or_default();
                let cab = bundle_state
                    .cabs
                    .entry(cab_name.to_string())
                    .or_insert_with(|| WritableCab::new(cab_name, flags));

                let offset = cab.append(data)?;
                let size: u32 = data.len().try_into().map_err(|_| {
                    UnityAssetError::format(format!(
                        "Streamed resource too large for u32 size: {}",
                        data.len()
                    ))
                })?;

                // Register as an external (UnityPy-style) on the embedded SerializedFile.
                let asset = bundle.assets.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset_index out of range: {} asset_index={}",
                        key.source.describe(),
                        asset_index
                    ))
                })?;
                let asset_state = bundle_state.assets.entry(asset_index).or_default();
                let external =
                    plan_external_registration(asset, Some(&asset_state.edits), &cab_path);
                apply_planned_external(&mut asset_state.edits, external);

                Ok(StreamedResourceWrite {
                    path: cab_path,
                    offset,
                    size,
                })
            }
            BinarySourceKind::SerializedFile => match &key.source {
                BinarySource::Path(_) => {
                    let (source_key, file) =
                        resolve_serialized_file_source(&self.binary_assets, &key.source)?;
                    let source_key = source_key.clone();

                    let file_name = match &source_key {
                        BinarySource::Path(p) => p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .ok_or_else(|| {
                                UnityAssetError::format(format!(
                                    "Invalid SerializedFile path: {}",
                                    p.to_string_lossy()
                                ))
                            })?
                            .to_string(),
                        BinarySource::ArchiveEntry { entry_name, .. } => {
                            std::path::Path::new(entry_name)
                                .file_name()
                                .and_then(|s| s.to_str())
                                .ok_or_else(|| {
                                    UnityAssetError::format(format!(
                                        "Invalid archive entry name: {}",
                                        entry_name
                                    ))
                                })?
                                .to_string()
                        }
                        BinarySource::WebEntry { .. } => unreachable!("handled below"),
                    };

                    // Use `archive:/{file_name}_data/{cab_name}` so `read_stream_data_from_fs` can
                    // resolve it via `base_dir.join(normalized)` after saving, without colliding
                    // with the `.assets` file itself on disk.
                    let cab_dir = format!("{file_name}_data");
                    let cab_path = format!("archive:/{}/{}", cab_dir, cab_name);

                    let file_state = self.write_state.standalone.entry(source_key).or_default();
                    let cab = file_state
                        .cabs
                        .entry(cab_name.to_string())
                        .or_insert_with(|| WritableCab::new(cab_name, 0x4));

                    let offset = cab.append(data)?;
                    let size: u32 = data.len().try_into().map_err(|_| {
                        UnityAssetError::format(format!(
                            "Streamed resource too large for u32 size: {}",
                            data.len()
                        ))
                    })?;

                    let external =
                        plan_external_registration(file, Some(&file_state.edits), &cab_path);
                    apply_planned_external(&mut file_state.edits, external);

                    Ok(StreamedResourceWrite {
                        path: cab_path,
                        offset,
                        size,
                    })
                }
                BinarySource::ArchiveEntry { .. } => {
                    let (source_key, file) =
                        resolve_serialized_file_source(&self.binary_assets, &key.source)?;
                    let source_key = source_key.clone();

                    let file_name = match &source_key {
                        BinarySource::Path(_) => unreachable!("handled above"),
                        BinarySource::ArchiveEntry { entry_name, .. } => {
                            std::path::Path::new(entry_name)
                                .file_name()
                                .and_then(|s| s.to_str())
                                .ok_or_else(|| {
                                    UnityAssetError::format(format!(
                                        "Invalid archive entry name: {}",
                                        entry_name
                                    ))
                                })?
                                .to_string()
                        }
                        BinarySource::WebEntry { .. } => unreachable!("handled below"),
                    };

                    let cab_dir = format!("{file_name}_data");
                    let cab_path = format!("archive:/{}/{}", cab_dir, cab_name);

                    let file_state = self.write_state.standalone.entry(source_key).or_default();
                    let cab = file_state
                        .cabs
                        .entry(cab_name.to_string())
                        .or_insert_with(|| WritableCab::new(cab_name, 0x4));

                    let offset = cab.append(data)?;
                    let size: u32 = data.len().try_into().map_err(|_| {
                        UnityAssetError::format(format!(
                            "Streamed resource too large for u32 size: {}",
                            data.len()
                        ))
                    })?;

                    let external =
                        plan_external_registration(file, Some(&file_state.edits), &cab_path);
                    apply_planned_external(&mut file_state.edits, external);

                    Ok(StreamedResourceWrite {
                        path: cab_path,
                        offset,
                        size,
                    })
                }
                BinarySource::WebEntry {
                    web_path,
                    entry_name,
                } => {
                    let (source_key, file) =
                        resolve_serialized_file_source(&self.binary_assets, &key.source)?;
                    let source_key = source_key.clone();

                    let cab_path = format!("archive:/{}/{}", entry_name, cab_name);
                    let web_path_key = super::path::canonicalize_if_exists(web_path);

                    let web_state = self.write_state.webfiles.entry(web_path_key).or_default();
                    let cab = web_state
                        .cabs
                        .entry(cab_name.to_string())
                        .or_insert_with(|| WritableCab::new(cab_name, 0));

                    let offset = cab.append(data)?;
                    let size: u32 = data.len().try_into().map_err(|_| {
                        UnityAssetError::format(format!(
                            "Streamed resource too large for u32 size: {}",
                            data.len()
                        ))
                    })?;

                    let file_state = self.write_state.standalone.entry(source_key).or_default();
                    let external =
                        plan_external_registration(file, Some(&file_state.edits), &cab_path);
                    apply_planned_external(&mut file_state.edits, external);

                    Ok(StreamedResourceWrite {
                        path: cab_path,
                        offset,
                        size,
                    })
                }
            },
        }
    }

    fn write_streamed_resource_transaction(
        &mut self,
        key: &BinaryObjectKey,
        cab_name: Option<&str>,
        data: &[u8],
        budget: &mut AssetLoadBudget,
        apply: impl FnOnce(&mut UnityClass, &StreamedResourceWrite) -> Result<()>,
    ) -> Result<StreamedResourceWrite> {
        let cab_name = cab_name.unwrap_or("CAB-UnityPy_Mod.resS");

        match key.source_kind {
            BinarySourceKind::AssetBundle => {
                let asset_index = key.asset_index.ok_or_else(|| {
                    UnityAssetError::format("AssetBundle key requires an asset_index")
                })?;
                let (bundle_source_key, bundle) =
                    resolve_bundle_source(&self.bundles, &key.source)?;
                let bundle_source_key = bundle_source_key.clone();
                let asset = bundle.assets.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset index out of range: {} asset_index={}",
                        key.source.describe(),
                        asset_index
                    ))
                })?;
                let node_name = bundle.asset_names.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset name missing: {} asset_index={}",
                        key.source.describe(),
                        asset_index
                    ))
                })?;
                let cab_path = format!("archive:/{}/{}", node_name, cab_name);
                let flags = bundle
                    .nodes
                    .iter()
                    .find(|node| {
                        node.is_file()
                            && (node.name.ends_with(".resS") || node.name.ends_with(".resource"))
                    })
                    .map(|node| node.flags)
                    .unwrap_or(0)
                    | 0x4;

                let existing_bundle = self.write_state.bundles.get(&bundle_source_key);
                let existing_asset =
                    existing_bundle.and_then(|state| state.assets.get(&asset_index));
                let planned = plan_streamed_resource_write(
                    existing_bundle.map(|state| &state.cabs),
                    asset,
                    existing_asset.map(|state| &state.edits),
                    cab_name,
                    cab_path,
                    data,
                )?;
                let write = planned.write;
                let prepared_object = prepare_serialized_file_edit(
                    asset,
                    existing_asset,
                    key.path_id,
                    budget,
                    |class| apply(class, &write),
                )?;
                let mut external = planned.external;

                if let Some(bundle_state) = self.write_state.bundles.get_mut(&bundle_source_key) {
                    commit_cab_append(&mut bundle_state.cabs, cab_name, flags, data)?;
                    let asset_state = bundle_state.assets.entry(asset_index).or_default();
                    apply_planned_external(&mut asset_state.edits, external.take());
                    apply_serialized_file_edit(asset_state, key.path_id, prepared_object);
                } else {
                    let mut bundle_state = BundleWriteState::default();
                    commit_cab_append(&mut bundle_state.cabs, cab_name, flags, data)?;
                    let asset_state = bundle_state.assets.entry(asset_index).or_default();
                    apply_planned_external(&mut asset_state.edits, external);
                    apply_serialized_file_edit(asset_state, key.path_id, prepared_object);
                    self.write_state
                        .bundles
                        .insert(bundle_source_key, bundle_state);
                }
                Ok(write)
            }
            BinarySourceKind::SerializedFile => {
                let (source_key, file) =
                    resolve_serialized_file_source(&self.binary_assets, &key.source)?;
                let source_key = source_key.clone();

                match &source_key {
                    BinarySource::Path(_) | BinarySource::ArchiveEntry { .. } => {
                        let file_name = standalone_cab_file_name(&source_key)?;
                        let cab_path = format!("archive:/{}_data/{}", file_name, cab_name);
                        let existing_file = self.write_state.standalone.get(&source_key);
                        let planned = plan_streamed_resource_write(
                            existing_file.map(|state| &state.cabs),
                            file,
                            existing_file.map(|state| &state.edits),
                            cab_name,
                            cab_path,
                            data,
                        )?;
                        let write = planned.write;
                        let prepared_object = prepare_serialized_file_edit(
                            file,
                            existing_file,
                            key.path_id,
                            budget,
                            |class| apply(class, &write),
                        )?;
                        let mut external = planned.external;

                        if let Some(file_state) = self.write_state.standalone.get_mut(&source_key) {
                            commit_cab_append(&mut file_state.cabs, cab_name, 0x4, data)?;
                            apply_planned_external(&mut file_state.edits, external.take());
                            apply_serialized_file_edit(file_state, key.path_id, prepared_object);
                        } else {
                            let mut file_state = SerializedFileWriteState::default();
                            commit_cab_append(&mut file_state.cabs, cab_name, 0x4, data)?;
                            apply_planned_external(&mut file_state.edits, external);
                            apply_serialized_file_edit(
                                &mut file_state,
                                key.path_id,
                                prepared_object,
                            );
                            self.write_state.standalone.insert(source_key, file_state);
                        }
                        Ok(write)
                    }
                    BinarySource::WebEntry {
                        web_path,
                        entry_name,
                    } => {
                        let cab_path = format!("archive:/{}/{}", entry_name, cab_name);
                        let web_path_key = super::path::canonicalize_if_exists(web_path);
                        let existing_web = self.write_state.webfiles.get(&web_path_key);
                        let existing_file = self.write_state.standalone.get(&source_key);
                        let planned = plan_streamed_resource_write(
                            existing_web.map(|state| &state.cabs),
                            file,
                            existing_file.map(|state| &state.edits),
                            cab_name,
                            cab_path,
                            data,
                        )?;
                        let write = planned.write;
                        let prepared_object = prepare_serialized_file_edit(
                            file,
                            existing_file,
                            key.path_id,
                            budget,
                            |class| apply(class, &write),
                        )?;
                        let new_web_state = if let Some(web_state) =
                            self.write_state.webfiles.get_mut(&web_path_key)
                        {
                            commit_cab_append(&mut web_state.cabs, cab_name, 0, data)?;
                            None
                        } else {
                            let mut web_state = WebFileWriteState::default();
                            commit_cab_append(&mut web_state.cabs, cab_name, 0, data)?;
                            Some(web_state)
                        };
                        let mut external = planned.external;
                        if let Some(file_state) = self.write_state.standalone.get_mut(&source_key) {
                            apply_planned_external(&mut file_state.edits, external.take());
                            apply_serialized_file_edit(file_state, key.path_id, prepared_object);
                        } else {
                            let mut file_state = SerializedFileWriteState::default();
                            apply_planned_external(&mut file_state.edits, external);
                            apply_serialized_file_edit(
                                &mut file_state,
                                key.path_id,
                                prepared_object,
                            );
                            self.write_state.standalone.insert(source_key, file_state);
                        }
                        if let Some(web_state) = new_web_state {
                            self.write_state.webfiles.insert(web_path_key, web_state);
                        }
                        Ok(write)
                    }
                }
            }
        }
    }

    /// Set a `PPtr` stored at a dot-separated field path (e.g. `m_RD.texture`) to point at `target_key`.
    ///
    /// This is a best-effort helper that computes the correct `fileID` relative to the context object:
    /// - `fileID=0` for targets inside the same serialized file
    /// - `fileID>0` for targets in other serialized files, by adding an external entry when missing
    pub fn set_pptr_path_to_key(
        &mut self,
        context_key: &BinaryObjectKey,
        pptr_path: &str,
        target_key: &BinaryObjectKey,
        budget: &mut AssetLoadBudget,
    ) -> Result<(i32, i64)> {
        let same_file = context_key.source_kind == target_key.source_kind
            && context_key.source == target_key.source
            && context_key.asset_index == target_key.asset_index;

        match context_key.source_kind {
            BinarySourceKind::SerializedFile => {
                let (source_key, file) =
                    resolve_serialized_file_source(&self.binary_assets, &context_key.source)?;
                let source_key = source_key.clone();
                let existing = self.write_state.standalone.get(&source_key);
                let external_plan = if same_file {
                    None
                } else {
                    let path = external_path_for_target(None, target_key)?;
                    Some(plan_external_file_id(
                        file,
                        existing.map(|state| &state.edits),
                        &path,
                    )?)
                };
                let file_id = external_plan.as_ref().map_or(0, |plan| plan.file_id);

                let prepared = prepare_serialized_file_edit(
                    file,
                    existing,
                    context_key.path_id,
                    budget,
                    |class| {
                        super::pptr_path::write_pptr_at_path(
                            class,
                            pptr_path,
                            file_id,
                            target_key.path_id,
                        )
                    },
                )?;
                let state = self.write_state.standalone.entry(source_key).or_default();
                if let Some(plan) = external_plan {
                    apply_external_file_id_plan(&mut state.edits, plan);
                }
                apply_serialized_file_edit(state, context_key.path_id, prepared);

                Ok((file_id, target_key.path_id))
            }
            BinarySourceKind::AssetBundle => {
                let asset_index = context_key.asset_index.ok_or_else(|| {
                    UnityAssetError::format("AssetBundle key requires an asset_index")
                })?;
                let (bundle_source, bundle) =
                    resolve_bundle_source(&self.bundles, &context_key.source)?;
                let bundle_source_key = bundle_source.clone();

                let asset = bundle.assets.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset index out of range: {} asset_index={}",
                        context_key.source.describe(),
                        asset_index
                    ))
                })?;

                let existing = self
                    .write_state
                    .bundles
                    .get(&bundle_source_key)
                    .and_then(|state| state.assets.get(&asset_index));
                let external_plan = if same_file {
                    None
                } else {
                    let path = external_path_for_target(Some((bundle_source, bundle)), target_key)?;
                    Some(plan_external_file_id(
                        asset,
                        existing.map(|state| &state.edits),
                        &path,
                    )?)
                };
                let file_id = external_plan.as_ref().map_or(0, |plan| plan.file_id);

                let prepared = prepare_serialized_file_edit(
                    asset,
                    existing,
                    context_key.path_id,
                    budget,
                    |class| {
                        super::pptr_path::write_pptr_at_path(
                            class,
                            pptr_path,
                            file_id,
                            target_key.path_id,
                        )
                    },
                )?;
                let state = self
                    .write_state
                    .bundles
                    .entry(bundle_source_key)
                    .or_default()
                    .assets
                    .entry(asset_index)
                    .or_default();
                if let Some(plan) = external_plan {
                    apply_external_file_id_plan(&mut state.edits, plan);
                }
                apply_serialized_file_edit(state, context_key.path_id, prepared);

                Ok((file_id, target_key.path_id))
            }
        }
    }

    /// Compute the `fileID` to use when writing a `PPtr` from `context_key` to `target_key`.
    ///
    /// - Returns `0` if both objects live in the same serialized file.
    /// - Otherwise appends an external entry (if needed) and returns its `fileID` (`index + 1`).
    pub fn file_id_for_target(
        &mut self,
        context_key: &BinaryObjectKey,
        target_key: &BinaryObjectKey,
    ) -> Result<i32> {
        let same_file = context_key.source_kind == target_key.source_kind
            && context_key.source == target_key.source
            && context_key.asset_index == target_key.asset_index;
        if same_file {
            return Ok(0);
        }

        match context_key.source_kind {
            BinarySourceKind::SerializedFile => {
                let (source_key, file) =
                    resolve_serialized_file_source(&self.binary_assets, &context_key.source)?;
                let source_key = source_key.clone();
                let state = self.write_state.standalone.entry(source_key).or_default();

                let path = external_path_for_target(None, target_key)?;
                get_or_add_external_file_id(file, &mut state.edits, &path)
            }
            BinarySourceKind::AssetBundle => {
                let asset_index = context_key.asset_index.ok_or_else(|| {
                    UnityAssetError::format("AssetBundle key requires an asset_index")
                })?;
                let (bundle_source, bundle) =
                    resolve_bundle_source(&self.bundles, &context_key.source)?;
                let bundle_source_key = bundle_source.clone();

                let asset = bundle.assets.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset index out of range: {} asset_index={}",
                        context_key.source.describe(),
                        asset_index
                    ))
                })?;

                let bundle_state = self
                    .write_state
                    .bundles
                    .entry(bundle_source_key)
                    .or_default();
                let state = bundle_state.assets.entry(asset_index).or_default();

                let path = external_path_for_target(Some((bundle_source, bundle)), target_key)?;
                get_or_add_external_file_id(asset, &mut state.edits, &path)
            }
        }
    }
}

struct PreparedSerializedFileEdit {
    class: UnityClass,
    bytes: Vec<u8>,
}

fn prepare_serialized_file_edit(
    file: &SerializedFile,
    state: Option<&SerializedFileWriteState>,
    path_id: i64,
    budget: &mut AssetLoadBudget,
    f: impl FnOnce(&mut UnityClass) -> Result<()>,
) -> Result<PreparedSerializedFileEdit> {
    let mut class = if let Some(existing) = state.and_then(|state| state.classes.get(&path_id)) {
        existing.clone()
    } else {
        let handle = file.find_object_handle(path_id).ok_or_else(|| {
            UnityAssetError::format(format!(
                "Object not found in SerializedFile: path_id={}",
                path_id
            ))
        })?;
        let parsed = handle.read(budget).map_err(|e| {
            UnityAssetError::with_source(
                format!("Failed to parse object for edit: path_id={}", path_id),
                e,
            )
        })?;
        parsed.class
    };

    f(&mut class)?;

    let mut session = SerializedFileEditSession::new(file);
    session.save_typetree(path_id, class.properties(), budget)?;
    let bytes = session
        .into_edits()
        .object_bytes
        .remove(&path_id)
        .ok_or_else(|| UnityAssetError::format("TypeTree edit produced no object bytes"))?;

    Ok(PreparedSerializedFileEdit { class, bytes })
}

fn apply_serialized_file_edit(
    state: &mut SerializedFileWriteState,
    path_id: i64,
    prepared: PreparedSerializedFileEdit,
) {
    state.classes.insert(path_id, prepared.class);
    state.edits.set_object_bytes(path_id, prepared.bytes);
}

struct PlannedStreamedResourceWrite {
    write: StreamedResourceWrite,
    external: Option<FileIdentifier>,
}

fn standalone_cab_file_name(source: &BinarySource) -> Result<&str> {
    match source {
        BinarySource::Path(path) => {
            path.file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Invalid SerializedFile path: {}",
                        path.to_string_lossy()
                    ))
                })
        }
        BinarySource::ArchiveEntry { entry_name, .. } => std::path::Path::new(entry_name)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                UnityAssetError::format(format!("Invalid archive entry name: {}", entry_name))
            }),
        BinarySource::WebEntry { .. } => Err(UnityAssetError::format(
            "WebEntry cabs are stored in their containing WebFile",
        )),
    }
}

fn plan_streamed_resource_write(
    cabs: Option<&HashMap<String, WritableCab>>,
    file: &SerializedFile,
    edits: Option<&SerializedFileEdits>,
    cab_name: &str,
    cab_path: String,
    data: &[u8],
) -> Result<PlannedStreamedResourceWrite> {
    let size: u32 = data.len().try_into().map_err(|_| {
        UnityAssetError::format(format!(
            "Streamed resource too large for u32 size: {}",
            data.len()
        ))
    })?;
    let existing_len = cabs
        .and_then(|cabs| cabs.get(cab_name))
        .map_or(0, |cab| cab.bytes().len());
    existing_len.checked_add(data.len()).ok_or_else(|| {
        UnityAssetError::format(format!(
            "WritableCab size overflow: existing={} appended={}",
            existing_len,
            data.len()
        ))
    })?;
    let offset = existing_len
        .try_into()
        .map_err(|_| UnityAssetError::format("WritableCab offset does not fit u64"))?;
    let external = plan_external_registration(file, edits, &cab_path);

    Ok(PlannedStreamedResourceWrite {
        write: StreamedResourceWrite {
            path: cab_path,
            offset,
            size,
        },
        external,
    })
}

fn commit_cab_append(
    cabs: &mut HashMap<String, WritableCab>,
    cab_name: &str,
    flags: u32,
    data: &[u8],
) -> Result<()> {
    if let Some(cab) = cabs.get_mut(cab_name) {
        cab.append(data)?;
        return Ok(());
    }

    let mut cab = WritableCab::new(cab_name, flags);
    cab.append(data)?;
    cabs.insert(cab_name.to_string(), cab);
    Ok(())
}

fn plan_external_registration(
    file: &SerializedFile,
    edits: Option<&SerializedFileEdits>,
    path: &str,
) -> Option<FileIdentifier> {
    if file.externals.iter().any(|external| external.path == path)
        || edits.is_some_and(|edits| {
            edits
                .additional_externals
                .iter()
                .any(|external| external.path == path)
        })
    {
        return None;
    }

    Some(FileIdentifier {
        temp_empty: String::new(),
        guid: pseudo_guid(),
        type_: 0,
        path: path.to_string(),
    })
}

fn apply_planned_external(edits: &mut SerializedFileEdits, external: Option<FileIdentifier>) {
    if let Some(external) = external {
        edits.add_external(external);
    }
}

fn expected_class_id_for_key(env: &Environment, key: &BinaryObjectKey) -> Result<i32> {
    match key.source_kind {
        BinarySourceKind::SerializedFile => {
            let (_, file) = resolve_serialized_file_source(&env.binary_assets, &key.source)?;
            let handle = file.find_object_handle(key.path_id).ok_or_else(|| {
                UnityAssetError::format(format!(
                    "Object not found in SerializedFile {}: path_id={}",
                    key.source.describe(),
                    key.path_id
                ))
            })?;
            Ok(handle.class_id())
        }
        BinarySourceKind::AssetBundle => {
            let asset_index = key.asset_index.ok_or_else(|| {
                UnityAssetError::format(
                    "AssetBundle key requires an asset_index (which asset in the bundle?)",
                )
            })?;
            let (_, bundle) = resolve_bundle_source(&env.bundles, &key.source)?;
            let file = bundle.assets.get(asset_index).ok_or_else(|| {
                UnityAssetError::format(format!(
                    "AssetBundle asset index out of range: {} asset_index={}",
                    key.source.describe(),
                    asset_index
                ))
            })?;
            let handle = file.find_object_handle(key.path_id).ok_or_else(|| {
                UnityAssetError::format(format!(
                    "Object not found in AssetBundle {} asset_index={}: path_id={}",
                    key.source.describe(),
                    asset_index,
                    key.path_id
                ))
            })?;
            Ok(handle.class_id())
        }
    }
}

fn resolve_serialized_file_source<'a>(
    assets: &'a HashMap<BinarySource, SerializedFile>,
    source: &BinarySource,
) -> Result<(&'a BinarySource, &'a SerializedFile)> {
    if let Some((k, v)) = assets.get_key_value(source) {
        return Ok((k, v));
    }

    if let Some(alt) = canonicalize_source_if_possible(source)
        && let Some((k, v)) = assets.get_key_value(&alt)
    {
        return Ok((k, v));
    }

    Err(UnityAssetError::format(format!(
        "SerializedFile source not loaded: {}",
        source.describe()
    )))
}

fn resolve_bundle_source<'a>(
    bundles: &'a HashMap<BinarySource, AssetBundle>,
    source: &BinarySource,
) -> Result<(&'a BinarySource, &'a AssetBundle)> {
    if let Some((k, v)) = bundles.get_key_value(source) {
        return Ok((k, v));
    }

    if let Some(alt) = canonicalize_source_if_possible(source)
        && let Some((k, v)) = bundles.get_key_value(&alt)
    {
        return Ok((k, v));
    }

    Err(UnityAssetError::format(format!(
        "AssetBundle source not loaded: {}",
        source.describe()
    )))
}

struct ExternalFileIdPlan {
    file_id: i32,
    external: Option<FileIdentifier>,
}

fn plan_external_file_id(
    file: &SerializedFile,
    edits: Option<&SerializedFileEdits>,
    path: &str,
) -> Result<ExternalFileIdPlan> {
    if let Some((idx, _)) = file
        .externals
        .iter()
        .enumerate()
        .find(|(_, e)| e.path == path)
    {
        return Ok(ExternalFileIdPlan {
            file_id: external_index_to_file_id(idx)?,
            external: None,
        });
    }

    if let Some((idx, _)) = edits.and_then(|edits| {
        edits
            .additional_externals
            .iter()
            .enumerate()
            .find(|(_, external)| external.path == path)
    }) {
        let index = file
            .externals
            .len()
            .checked_add(idx)
            .ok_or_else(|| UnityAssetError::format("External file index overflow"))?;
        return Ok(ExternalFileIdPlan {
            file_id: external_index_to_file_id(index)?,
            external: None,
        });
    }

    let pending = edits.map_or(0, |edits| edits.additional_externals.len());
    let index = file
        .externals
        .len()
        .checked_add(pending)
        .ok_or_else(|| UnityAssetError::format("External file index overflow"))?;
    Ok(ExternalFileIdPlan {
        file_id: external_index_to_file_id(index)?,
        external: Some(FileIdentifier {
            temp_empty: String::new(),
            guid: pseudo_guid(),
            type_: 0,
            path: path.to_string(),
        }),
    })
}

fn apply_external_file_id_plan(edits: &mut SerializedFileEdits, plan: ExternalFileIdPlan) -> i32 {
    if let Some(external) = plan.external {
        edits.add_external(external);
    }
    plan.file_id
}

fn get_or_add_external_file_id(
    file: &SerializedFile,
    edits: &mut SerializedFileEdits,
    path: &str,
) -> Result<i32> {
    let plan = plan_external_file_id(file, Some(edits), path)?;
    Ok(apply_external_file_id_plan(edits, plan))
}

fn external_index_to_file_id(index: usize) -> Result<i32> {
    index
        .checked_add(1)
        .and_then(|file_id| i32::try_from(file_id).ok())
        .ok_or_else(|| UnityAssetError::format("External file ID does not fit i32"))
}

fn external_path_for_target(
    bundle: Option<(&BinarySource, &AssetBundle)>,
    target_key: &BinaryObjectKey,
) -> Result<String> {
    if let Some((bundle_source, bundle)) = bundle
        && target_key.source_kind == BinarySourceKind::AssetBundle
        && &target_key.source == bundle_source
        && let Some(target_asset_index) = target_key.asset_index
        && let Some(name) = bundle.asset_names.get(target_asset_index)
    {
        return Ok(name.clone());
    }

    match &target_key.source {
        BinarySource::Path(p) => Ok(p
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| p.to_string_lossy().to_string())),
        BinarySource::ArchiveEntry { entry_name, .. } => Ok(std::path::Path::new(entry_name)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| entry_name.clone())),
        BinarySource::WebEntry { entry_name, .. } => Ok(entry_name.clone()),
    }
}

fn pseudo_guid() -> [u8; 16] {
    let mut guid = [0u8; 16];
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut x = nanos as u64 ^ (nanos >> 64) as u64;
    for chunk in guid.chunks_mut(8) {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x = x.wrapping_mul(0x2545F4914F6CDD1D);
        chunk.copy_from_slice(&x.to_le_bytes());
    }
    guid
}
