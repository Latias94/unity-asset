use super::*;

impl Environment {
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

        // Best-effort: if the context object comes from a bundle, resolve external references to other
        // serialized files inside the same bundle.
        if context.source_kind == BinarySourceKind::AssetBundle {
            if let Some(bundle) = self.bundles.get(context.source) {
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
}
