use super::*;

#[derive(Debug, Clone, Copy)]
pub struct PptrReferenceSearchOptions {
    pub continue_on_error: bool,
    pub max_objects: Option<usize>,
    pub max_results: Option<usize>,
    pub max_pptrs_per_object: Option<usize>,
}

impl Default for PptrReferenceSearchOptions {
    fn default() -> Self {
        Self {
            continue_on_error: true,
            max_objects: None,
            max_results: None,
            max_pptrs_per_object: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryPptrReference {
    pub from: BinaryObjectKey,
    pub pptr_path: String,
    pub file_id: i32,
    pub path_id: i64,
    pub resolved: Option<BinaryObjectKey>,
}

pub(crate) fn match_external_path_score(external_path: &str, candidate: &str) -> i32 {
    if external_path.is_empty() || candidate.is_empty() {
        return 0;
    }

    let a = external_path.replace('\\', "/");
    let b = candidate.replace('\\', "/");

    if a == b {
        return 300;
    }
    if a.eq_ignore_ascii_case(&b) {
        return 250;
    }

    if a.ends_with(&b) || b.ends_with(&a) {
        return 200;
    }

    let a_name = std::path::Path::new(&a)
        .file_name()
        .and_then(|n| n.to_str());
    let b_name = std::path::Path::new(&b)
        .file_name()
        .and_then(|n| n.to_str());
    if let (Some(a_name), Some(b_name)) = (a_name, b_name) {
        if a_name == b_name {
            return 150;
        }
        if a_name.eq_ignore_ascii_case(b_name) {
            return 120;
        }
    }

    0
}

#[derive(Debug, Clone)]
struct ExternalHint {
    path: Option<String>,
    guid: Option<[u8; 16]>,
}

trait ExternalHintLike {
    fn path(&self) -> Option<&str>;
    fn guid(&self) -> Option<[u8; 16]>;
}

impl ExternalHintLike for ExternalHint {
    fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    fn guid(&self) -> Option<[u8; 16]> {
        self.guid
    }
}

fn bundle_source_simple_name(source: &BinarySource) -> Option<String> {
    match source {
        BinarySource::Path(p) => p
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string()),
        BinarySource::ArchiveEntry { entry_name, .. } => std::path::Path::new(entry_name)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string()),
        BinarySource::WebEntry { entry_name, .. } => std::path::Path::new(entry_name)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string()),
    }
}

impl Environment {
    // NOTE: path scoring lives at module scope so it can be reused by dependency resolution code.

    fn find_loaded_bundle_asset_by_external_path(
        &self,
        external_path: &str,
    ) -> Option<(BinarySource, usize)> {
        if external_path.is_empty() {
            return None;
        }

        let mut best_score = 0i32;
        let mut best: Vec<(BinarySource, usize)> = Vec::new();

        let mut bundle_sources: Vec<&BinarySource> = self.bundles.keys().collect();
        bundle_sources.sort();

        for source in bundle_sources {
            let Some(bundle) = self.bundles.get(source) else {
                continue;
            };

            // UnityPy's externals sometimes store the *file name* of the referenced bundle, not a
            // bundle-internal node name. Best-effort: if this bundle has exactly one serialized
            // file, allow matching by bundle source name and map it to asset_index=0.
            if bundle.assets.len() == 1 {
                let Some(source_name) = bundle_source_simple_name(source) else {
                    continue;
                };
                let score = match_external_path_score(external_path, &source_name);
                if score > 0 {
                    if score > best_score {
                        best_score = score;
                        best.clear();
                        best.push((source.clone(), 0));
                    } else if score == best_score {
                        best.push((source.clone(), 0));
                    }
                }
            }

            for (idx, name) in bundle.asset_names.iter().enumerate() {
                let score = match_external_path_score(external_path, name);
                if score == 0 {
                    continue;
                }
                if score > best_score {
                    best_score = score;
                    best.clear();
                    best.push((source.clone(), idx));
                } else if score == best_score {
                    best.push((source.clone(), idx));
                }
            }
        }

        match best.as_slice() {
            [(source, idx)] => Some((source.clone(), *idx)),
            _ => None,
        }
    }

    fn find_loaded_serialized_source_by_external_path(
        &self,
        external_path: &str,
    ) -> Option<BinarySource> {
        if external_path.is_empty() {
            return None;
        }

        let direct = Path::new(external_path);
        let direct_key = BinarySource::Path(direct.to_path_buf());
        if self.binary_assets.contains_key(&direct_key) {
            return Some(direct_key);
        }

        if !direct.is_absolute() {
            let joined = self.base_path.join(direct);
            let joined_key = BinarySource::Path(joined);
            if self.binary_assets.contains_key(&joined_key) {
                return Some(joined_key);
            }
        }

        let target_file_name = direct.file_name().and_then(|n| n.to_str());
        let mut by_name: Vec<&PathBuf> = Vec::new();
        if let Some(name) = target_file_name {
            by_name.extend(
                self.binary_assets
                    .keys()
                    .filter_map(|k| k.as_path())
                    .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(name)),
            );
        }
        by_name.sort();
        if let Some(found) = by_name.first() {
            return Some(BinarySource::Path((*found).clone()));
        }

        let external_norm = external_path.replace('\\', "/");
        let mut by_suffix: Vec<&PathBuf> = self
            .binary_assets
            .keys()
            .filter_map(|k| k.as_path())
            .filter(|p| {
                let p_str = p.to_string_lossy().replace('\\', "/");
                p_str.ends_with(&external_norm) || external_norm.ends_with(&p_str)
            })
            .collect();
        by_suffix.sort();
        by_suffix.first().cloned().cloned().map(BinarySource::Path)
    }

    /// Resolve a Unity `PPtr` (`fileID`, `pathID`) into a globally-unique object key.
    ///
    /// Notes:
    /// - `file_id == 0` points to the same `SerializedFile` as the context object.
    /// - `file_id > 0` indexes into the context file's `externals` list (Unity convention: `file_id - 1`).
    /// - External resolution is best-effort and currently only matches already-loaded standalone serialized files.
    pub fn resolve_binary_pptr(
        &self,
        context: &BinaryObjectRef<'_>,
        file_id: i32,
        path_id: i64,
    ) -> Option<BinaryObjectKey> {
        if file_id == 0 {
            return Some(BinaryObjectKey {
                source: context.source.clone(),
                source_kind: context.source_kind,
                asset_index: context.asset_index,
                path_id,
            });
        }

        if file_id < 0 {
            return None;
        }

        let idx: usize = (file_id - 1).try_into().ok()?;
        let external = context.object.file().externals.get(idx)?;

        // Best-effort: GUID-based resolution via loaded `.meta` files.
        if external.guid != [0u8; 16]
            && let Some(asset_path) = self.asset_path_for_guid(external.guid)
        {
            let direct_key = BinarySource::Path(asset_path.clone());
            if self.binary_assets.contains_key(&direct_key) {
                return Some(BinaryObjectKey {
                    source: direct_key,
                    source_kind: BinarySourceKind::SerializedFile,
                    asset_index: None,
                    path_id,
                });
            }
        }

        // Best-effort: if the context object comes from a bundle, resolve external references to other
        // serialized files inside the same bundle.
        if context.source_kind == BinarySourceKind::AssetBundle
            && let Some(bundle) = self.bundles.get(context.source)
        {
            let external_norm = external.path.replace('\\', "/");
            let external_file_name = std::path::Path::new(&external_norm)
                .file_name()
                .and_then(|n| n.to_str());

            let mut candidates: Vec<(usize, &String)> =
                bundle.asset_names.iter().enumerate().collect();
            candidates.sort_by(|a, b| a.1.cmp(b.1));

            if let Some((asset_index, _)) = candidates.into_iter().find(|(_, name)| {
                let name_norm = name.replace('\\', "/");
                if name_norm == external_norm {
                    return true;
                }
                if name_norm.ends_with(&external_norm) || external_norm.ends_with(&name_norm) {
                    return true;
                }
                match external_file_name {
                    Some(file_name) => {
                        std::path::Path::new(&name_norm)
                            .file_name()
                            .and_then(|n| n.to_str())
                            == Some(file_name)
                    }
                    None => false,
                }
            }) {
                return Some(BinaryObjectKey {
                    source: context.source.clone(),
                    source_kind: BinarySourceKind::AssetBundle,
                    asset_index: Some(asset_index),
                    path_id,
                });
            }
        }

        // Best-effort: resolve external references to serialized files inside *any* loaded bundle.
        if let Some((bundle_source, asset_index)) =
            self.find_loaded_bundle_asset_by_external_path(&external.path)
        {
            return Some(BinaryObjectKey {
                source: bundle_source,
                source_kind: BinarySourceKind::AssetBundle,
                asset_index: Some(asset_index),
                path_id,
            });
        }

        // Fallback: resolve to an already-loaded standalone serialized file on disk.
        let resolved_source =
            self.find_loaded_serialized_source_by_external_path(&external.path)?;
        Some(BinaryObjectKey {
            source: resolved_source,
            source_kind: BinarySourceKind::SerializedFile,
            asset_index: None,
            path_id,
        })
    }

    /// Resolve and parse a Unity `PPtr` (`fileID`, `pathID`) using a context object for external mapping.
    pub fn read_binary_pptr(
        &self,
        context: &BinaryObjectRef<'_>,
        file_id: i32,
        path_id: i64,
    ) -> Result<UnityObject> {
        let key = self
            .resolve_binary_pptr(context, file_id, path_id)
            .ok_or_else(|| {
                UnityAssetError::format(format!(
                    "Failed to resolve PPtr: file_id={}, path_id={}",
                    file_id, path_id
                ))
            })?;
        self.read_binary_object_key(&key)
    }

    fn binary_object_ref_for_key(&self, key: &BinaryObjectKey) -> Result<BinaryObjectRef<'_>> {
        match key.source_kind {
            BinarySourceKind::SerializedFile => self
                .find_binary_object_in_source_id(&key.source, key.path_id)
                .ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Object not found in SerializedFile {}: path_id={}",
                        key.source.describe(),
                        key.path_id
                    ))
                }),
            BinarySourceKind::AssetBundle => {
                let asset_index = key.asset_index.ok_or_else(|| {
                    UnityAssetError::format("AssetBundle key requires an asset_index")
                })?;
                self.find_binary_object_in_bundle_asset_source(
                    &key.source,
                    asset_index,
                    key.path_id,
                )
                .ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Object not found in AssetBundle {} asset_index={} path_id={}",
                        key.source.describe(),
                        asset_index,
                        key.path_id
                    ))
                })
            }
        }
    }

    /// Resolve a `PPtr` stored at a dot-separated field path (e.g. `m_RD.texture`) to a globally-unique object key.
    pub fn resolve_pptr_path_key(
        &self,
        context_key: &BinaryObjectKey,
        pptr_path: &str,
    ) -> Result<Option<BinaryObjectKey>> {
        let obj_ref = self.binary_object_ref_for_key(context_key)?;
        let obj = obj_ref.read()?;

        let Some(v) = super::pptr_path::get_value_at_path(obj.as_unity_class(), pptr_path) else {
            return Ok(None);
        };
        let Some((file_id, path_id)) = super::pptr_path::read_pptr(v) else {
            return Ok(None);
        };
        if path_id == 0 {
            return Ok(None);
        }

        Ok(self.resolve_binary_pptr(&obj_ref, file_id, path_id))
    }

    /// Resolve a `PPtr` stored at a dot-separated field path, loading missing dependencies best-effort.
    ///
    /// This is closer to UnityPy's behavior where dereferencing a `PPtr` may trigger dependency loads
    /// through `Environment.find_file(...)`.
    pub fn resolve_pptr_path_key_best_effort(
        &mut self,
        context_key: &BinaryObjectKey,
        pptr_path: &str,
    ) -> Result<Option<BinaryObjectKey>> {
        let (file_id, path_id, hint) = {
            let obj_ref = self.binary_object_ref_for_key(context_key)?;
            let obj = obj_ref.read()?;

            let Some(v) = super::pptr_path::get_value_at_path(obj.as_unity_class(), pptr_path)
            else {
                return Ok(None);
            };
            let Some((file_id, path_id)) = super::pptr_path::read_pptr(v) else {
                return Ok(None);
            };
            if path_id == 0 {
                return Ok(None);
            }

            let hint = if file_id > 0 {
                let idx = usize::try_from(file_id - 1).ok().unwrap_or(usize::MAX);
                obj_ref
                    .object
                    .file()
                    .externals
                    .get(idx)
                    .map(|ext| ExternalHint {
                        path: if ext.path.is_empty() {
                            None
                        } else {
                            Some(ext.path.clone())
                        },
                        guid: if ext.guid == [0u8; 16] {
                            None
                        } else {
                            Some(ext.guid)
                        },
                    })
            } else {
                None
            };

            (file_id, path_id, hint)
        };

        // First attempt: resolve without loading anything.
        if let Ok(obj_ref) = self.binary_object_ref_for_key(context_key)
            && let Some(resolved) = self.resolve_binary_pptr(&obj_ref, file_id, path_id)
        {
            return Ok(Some(resolved));
        }

        // Best-effort: load dependency by GUID or path and retry.
        if let Some(hint) = hint {
            self.try_load_dependency_for_external_hint(&hint);
        }

        let obj_ref = self.binary_object_ref_for_key(context_key)?;
        Ok(self.resolve_binary_pptr(&obj_ref, file_id, path_id))
    }

    fn try_load_dependency_for_external_hint(&mut self, hint: &impl ExternalHintLike) {
        // Preserve base_path (UnityPy's Environment.path does not change during dependency loads).
        let saved_base_path = self.base_path.clone();

        if let Some(guid) = hint.guid() {
            if let Some(asset_path) = self.asset_path_for_guid(guid) {
                let source = BinarySource::path(&asset_path);
                if !self.binary_assets.contains_key(&source) && !self.bundles.contains_key(&source)
                {
                    if let Err(e) = self.load_file(&asset_path) {
                        self.push_warning(EnvironmentWarning::LoadFailed {
                            path: asset_path,
                            error: e.to_string(),
                        });
                    }
                }
            }
        }

        if let Some(path) = hint.path() {
            if let Some(found) = self.find_dependency_path_best_effort(path) {
                let source = BinarySource::path(&found);
                if !self.binary_assets.contains_key(&source) && !self.bundles.contains_key(&source)
                {
                    if let Err(e) = self.load_file(&found) {
                        self.push_warning(EnvironmentWarning::LoadFailed {
                            path: found,
                            error: e.to_string(),
                        });
                    }
                }
            }
        }

        self.base_path = saved_base_path;
    }

    pub fn find_binary_pptr_references_to(
        &self,
        target_key: &BinaryObjectKey,
        options: PptrReferenceSearchOptions,
    ) -> Result<Vec<BinaryPptrReference>> {
        let mut out: Vec<BinaryPptrReference> = Vec::new();

        let mut scanned_objects = 0usize;
        for obj_ref in self.binary_object_infos() {
            if let Some(max) = options.max_objects
                && scanned_objects >= max
            {
                break;
            }
            if let Some(max) = options.max_results
                && out.len() >= max
            {
                break;
            }
            scanned_objects = scanned_objects.saturating_add(1);

            let from_key = obj_ref.key();

            let class = match obj_ref.source_kind {
                BinarySourceKind::SerializedFile => {
                    if let Some(state) = self.write_state.standalone.get(obj_ref.source)
                        && let Some(class) = state.classes.get(&from_key.path_id)
                    {
                        class.clone()
                    } else {
                        match obj_ref.read() {
                            Ok(obj) => obj.class,
                            Err(e) => {
                                if options.continue_on_error {
                                    continue;
                                }
                                return Err(e);
                            }
                        }
                    }
                }
                BinarySourceKind::AssetBundle => {
                    let Some(asset_index) = obj_ref.asset_index else {
                        if options.continue_on_error {
                            continue;
                        }
                        return Err(UnityAssetError::format(
                            "AssetBundle object ref missing asset_index".to_string(),
                        ));
                    };

                    if let Some(bundle_state) = self.write_state.bundles.get(obj_ref.source)
                        && let Some(asset_state) = bundle_state.assets.get(&asset_index)
                        && let Some(class) = asset_state.classes.get(&from_key.path_id)
                    {
                        class.clone()
                    } else {
                        match obj_ref.read() {
                            Ok(obj) => obj.class,
                            Err(e) => {
                                if options.continue_on_error {
                                    continue;
                                }
                                return Err(e);
                            }
                        }
                    }
                }
            };

            let pptrs =
                super::pptr_path::scan_pptrs_with_paths(&class, options.max_pptrs_per_object);
            for p in pptrs {
                if p.path_id == 0 {
                    continue;
                }

                let resolved = self.resolve_binary_pptr(&obj_ref, p.file_id, p.path_id);
                if resolved.as_ref() != Some(target_key) {
                    continue;
                }

                out.push(BinaryPptrReference {
                    from: from_key.clone(),
                    pptr_path: p.path,
                    file_id: p.file_id,
                    path_id: p.path_id,
                    resolved,
                });

                if let Some(max) = options.max_results
                    && out.len() >= max
                {
                    break;
                }
            }
        }

        out.sort_by(|a, b| {
            a.from
                .to_string()
                .cmp(&b.from.to_string())
                .then_with(|| a.pptr_path.cmp(&b.pptr_path))
        });
        Ok(out)
    }
}
