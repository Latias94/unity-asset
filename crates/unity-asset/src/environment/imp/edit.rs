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
        self.standalone.is_empty() && self.bundles.is_empty() && self.webfiles.is_empty()
    }
}

/// A UnityPy-like edit session that records changes inside an `Environment`.
///
/// This is a convenience wrapper around `Environment` mutation APIs. Calling `save(...)` on the
/// environment will write only changed sources and then clear the pending edits.
pub struct EnvironmentEditSession<'a> {
    env: &'a mut Environment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedResourceWrite {
    pub path: String,
    pub offset: u64,
    pub size: u32,
}

impl<'a> EnvironmentEditSession<'a> {
    pub fn new(env: &'a mut Environment) -> Self {
        Self { env }
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
        self.env.edit_binary_object_key(key, f)
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
        let write = self.write_to_cab(key, cab_name, data)?;
        self.edit_binary_object_key(key, |class| {
            super::streamed_write::apply_streamed_resource_write(class, field_name, &write)
        })?;
        Ok(write)
    }

    pub fn save<P: AsRef<Path>>(
        &mut self,
        pack: unity_asset_write::PackerOptions,
        out_dir: P,
    ) -> Result<()> {
        self.env.save(pack, out_dir)
    }
}

impl Environment {
    pub fn edit_session(&mut self) -> EnvironmentEditSession<'_> {
        EnvironmentEditSession::new(self)
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
        f: impl FnOnce(&mut UnityClass) -> Result<()>,
    ) -> Result<()> {
        match key.source_kind {
            BinarySourceKind::SerializedFile => {
                let (source_key, file) =
                    resolve_serialized_file_source(&self.binary_assets, &key.source)?;
                let source_key = source_key.clone();
                let state = self.write_state.standalone.entry(source_key).or_default();
                edit_in_serialized_file(file, state, key.path_id, f)?;
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

                let bundle_state = self
                    .write_state
                    .bundles
                    .entry(bundle_source_key)
                    .or_default();
                let state = bundle_state.assets.entry(asset_index).or_default();

                edit_in_serialized_file(asset, state, key.path_id, f)?;
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
                register_external_if_missing(asset, &mut asset_state.edits, &cab_path);

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

                    register_external_if_missing(file, &mut file_state.edits, &cab_path);

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
                    register_external_if_missing(file, &mut file_state.edits, &cab_path);

                    Ok(StreamedResourceWrite {
                        path: cab_path,
                        offset,
                        size,
                    })
                }
            },
        }
    }
}

fn edit_in_serialized_file(
    file: &SerializedFile,
    state: &mut SerializedFileWriteState,
    path_id: i64,
    f: impl FnOnce(&mut UnityClass) -> Result<()>,
) -> Result<()> {
    let class = if let Some(existing) = state.classes.get_mut(&path_id) {
        existing
    } else {
        let handle = file.find_object_handle(path_id).ok_or_else(|| {
            UnityAssetError::format(format!(
                "Object not found in SerializedFile: path_id={}",
                path_id
            ))
        })?;
        let parsed = handle.read().map_err(|e| {
            UnityAssetError::with_source(
                format!("Failed to parse object for edit: path_id={}", path_id),
                e,
            )
        })?;
        state.classes.insert(path_id, parsed.class);
        state.classes.get_mut(&path_id).expect("just inserted")
    };

    f(class)?;

    // Always re-encode the full properties map and store bytes (UnityPy-style override).
    let mut session = SerializedFileEditSession::new(file);
    session.save_typetree(path_id, class.properties())?;

    // Merge the latest bytes into the Environment state.
    if let Some(bytes) = session.edits().get(path_id) {
        state.edits.set_object_bytes(path_id, bytes.to_vec());
    }

    Ok(())
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

fn register_external_if_missing(
    file: &SerializedFile,
    edits: &mut SerializedFileEdits,
    path: &str,
) {
    if file.externals.iter().any(|e| e.path == path) {
        return;
    }
    if edits.additional_externals.iter().any(|e| e.path == path) {
        return;
    }

    let guid = pseudo_guid();
    edits.add_external(FileIdentifier {
        temp_empty: String::new(),
        guid,
        type_: 0,
        path: path.to_string(),
    });
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
