use super::path::canonicalize_if_exists;
use super::*;

impl Environment {
    fn parse_glob_pattern(pattern: &str) -> Vec<GlobToken> {
        let mut out = Vec::new();
        let mut chars = pattern.chars().peekable();

        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    if let Some(next) = chars.next() {
                        out.push(GlobToken::Literal(next));
                    } else {
                        out.push(GlobToken::Literal('\\'));
                    }
                }
                '*' => {
                    if !matches!(out.last(), Some(GlobToken::Star)) {
                        out.push(GlobToken::Star);
                    }
                }
                '?' => out.push(GlobToken::AnyChar),
                other => out.push(GlobToken::Literal(other)),
            }
        }

        out
    }

    fn glob_match(tokens: &[GlobToken], text: &str) -> bool {
        let text: Vec<char> = text.chars().collect();

        let mut token_index = 0usize;
        let mut text_index = 0usize;

        let mut last_star: Option<usize> = None;
        let mut star_text_index = 0usize;

        while text_index < text.len() {
            match tokens.get(token_index) {
                Some(GlobToken::Literal(ch)) if *ch == text[text_index] => {
                    token_index += 1;
                    text_index += 1;
                }
                Some(GlobToken::AnyChar) => {
                    token_index += 1;
                    text_index += 1;
                }
                Some(GlobToken::Star) => {
                    last_star = Some(token_index);
                    token_index += 1;
                    star_text_index = text_index;
                }
                _ => {
                    if let Some(star) = last_star {
                        star_text_index += 1;
                        text_index = star_text_index;
                        token_index = star + 1;
                    } else {
                        return false;
                    }
                }
            }
        }

        while matches!(tokens.get(token_index), Some(GlobToken::Star)) {
            token_index += 1;
        }

        token_index == tokens.len()
    }

    fn scan_pptr(value: &UnityValue) -> Option<(i32, i64)> {
        match value {
            UnityValue::Object(obj) => {
                let file_id = obj
                    .get("fileID")
                    .or_else(|| obj.get("m_FileID"))
                    .and_then(|v| v.as_i64())
                    .and_then(|v| i32::try_from(v).ok());
                let path_id = obj
                    .get("pathID")
                    .or_else(|| obj.get("m_PathID"))
                    .and_then(|v| v.as_i64());

                if let (Some(file_id), Some(path_id)) = (file_id, path_id) {
                    return Some((file_id, path_id));
                }

                for (_, v) in obj.iter() {
                    if let Some(pptr) = Self::scan_pptr(v) {
                        return Some(pptr);
                    }
                }

                None
            }
            UnityValue::Array(items) => {
                for item in items {
                    if let Some(pptr) = Self::scan_pptr(item) {
                        return Some(pptr);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn extract_assetbundle_container_from_typetree(
        &self,
        context: &BinaryObjectRef<'_>,
        parsed: &UnityObject,
    ) -> Vec<BundleContainerEntry> {
        let mut out = Vec::new();

        let Some(UnityValue::Array(items)) = parsed.class.get("m_Container") else {
            return out;
        };

        for item in items {
            let (asset_path, second) = match item {
                // Unity typetree `pair` is represented as `[first, second]` by our TypeTree deserializer.
                UnityValue::Array(pair) if pair.len() == 2 => {
                    let Some(asset_path) = pair[0].as_str() else {
                        continue;
                    };
                    (asset_path.to_string(), &pair[1])
                }
                // Best-effort fallback for alternative pair representations.
                UnityValue::Object(obj) => {
                    let first = obj.get("first").and_then(|v| v.as_str());
                    let second = obj.get("second").or_else(|| obj.get("value"));
                    let (Some(first), Some(second)) = (first, second) else {
                        continue;
                    };
                    (first.to_string(), second)
                }
                _ => continue,
            };

            let Some((file_id, path_id)) = Self::scan_pptr(second) else {
                continue;
            };

            let key = if path_id == 0 {
                None
            } else {
                self.resolve_binary_pptr(context, file_id, path_id)
            };
            out.push(BundleContainerEntry {
                bundle_source: context.source.clone(),
                asset_index: context.asset_index.unwrap_or(0),
                asset_path,
                file_id,
                path_id,
                key,
            });
        }

        out
    }

    /// Extract best-effort `m_Container` entries from a loaded bundle source path.
    ///
    /// This scans for `AssetBundle` objects (class id `142`) inside the bundle and parses them to find
    /// `m_Container` entries.
    pub fn bundle_container_entries<P: AsRef<Path>>(
        &self,
        bundle_path: P,
    ) -> Result<Vec<BundleContainerEntry>> {
        let bundle_path = canonicalize_if_exists(bundle_path.as_ref());
        let bundle_source = BinarySource::path(&bundle_path);
        self.bundle_container_entries_source(&bundle_source)
    }

    pub fn bundle_container_entries_source(
        &self,
        bundle_source: &BinarySource,
    ) -> Result<Vec<BundleContainerEntry>> {
        match self.bundle_container_cache.read() {
            Ok(cache) => {
                if let Some(cached) = cache.get(bundle_source) {
                    return Ok(cached.clone());
                }
            }
            Err(e) => {
                let cache = e.into_inner();
                if let Some(cached) = cache.get(bundle_source) {
                    return Ok(cached.clone());
                }
            }
        }

        let (key, bundle) = self.bundles.get_key_value(bundle_source).ok_or_else(|| {
            UnityAssetError::format(format!(
                "AssetBundle source not loaded: {}",
                bundle_source.describe()
            ))
        })?;

        let mut out: Vec<BundleContainerEntry> = Vec::new();
        let typetree_options = self.options.typetree;
        let reporter = self.reporter.clone();

        for (asset_index, file) in bundle.assets.iter().enumerate() {
            for object in file.object_handles() {
                if object.class_id() != 142 {
                    continue;
                }
                let obj_ref = BinaryObjectRef {
                    source: key,
                    source_kind: BinarySourceKind::AssetBundle,
                    asset_index: Some(asset_index),
                    object,
                    typetree_options,
                    reporter: reporter.clone(),
                };

                // First, try TypeTree extraction when available.
                if object.file().enable_type_tree
                    && let Ok(parsed) = obj_ref.read()
                {
                    let extracted =
                        self.extract_assetbundle_container_from_typetree(&obj_ref, &parsed);
                    if !extracted.is_empty() {
                        out.extend(extracted);
                        continue;
                    }
                }

                // Fallback: raw parsing for stripped TypeTree bundles.
                if let Ok(raw_entries) = object.file().assetbundle_container_raw(object.info()) {
                    for (asset_path, file_id, path_id) in raw_entries {
                        let key = if path_id == 0 {
                            None
                        } else {
                            self.resolve_binary_pptr(&obj_ref, file_id, path_id)
                                .or_else(|| {
                                    // Fallback: if external mapping fails, try to locate the object by `path_id`
                                    // within the same bundle. This is best-effort and only used when `file_id`
                                    // can't be resolved.
                                    let matches = self
                                        .find_binary_objects_in_source_id(obj_ref.source, path_id);
                                    if matches.len() == 1 {
                                        Some(matches[0].key())
                                    } else {
                                        None
                                    }
                                })
                        };
                        out.push(BundleContainerEntry {
                            bundle_source: obj_ref.source.clone(),
                            asset_index,
                            asset_path,
                            file_id,
                            path_id,
                            key,
                        });
                    }
                }
            }
        }

        match self.bundle_container_cache.write() {
            Ok(mut cache) => {
                cache.insert(bundle_source.clone(), out.clone());
            }
            Err(e) => {
                e.into_inner().insert(bundle_source.clone(), out.clone());
            }
        }
        Ok(out)
    }

    /// Find container entries across all loaded bundles by `asset_path` pattern (case-insensitive ASCII).
    ///
    /// - When `pattern` contains `*` or `?`, it is treated as a glob.
    /// - Otherwise it is treated as a substring match.
    pub fn find_bundle_container_entries(&self, pattern: &str) -> Vec<BundleContainerEntry> {
        let mut bundle_sources: Vec<&BinarySource> = self.bundles.keys().collect();
        bundle_sources.sort();

        let pattern_lc = pattern.to_ascii_lowercase();
        let pattern_is_glob = pattern_lc.contains('*') || pattern_lc.contains('?');
        let glob = pattern_is_glob.then(|| Self::parse_glob_pattern(&pattern_lc));

        let mut out = Vec::new();
        for bundle_source in bundle_sources {
            if let Ok(entries) = self.bundle_container_entries_source(bundle_source) {
                out.extend(entries.into_iter().filter(|e| {
                    if pattern_lc.is_empty() {
                        return true;
                    }
                    let asset_path_lc = e.asset_path.to_ascii_lowercase();
                    if let Some(glob) = glob.as_ref() {
                        Self::glob_match(glob, &asset_path_lc)
                    } else {
                        asset_path_lc.contains(&pattern_lc)
                    }
                }));
            }
        }
        out
    }

    /// Find resolved `BinaryObjectKey`s from bundle containers by path substring.
    pub fn find_binary_object_keys_in_bundle_container(
        &self,
        pattern: &str,
    ) -> Vec<(String, BinaryObjectKey)> {
        self.find_bundle_container_entries(pattern)
            .into_iter()
            .filter_map(|e| e.key.map(|k| (e.asset_path, k)))
            .collect()
    }

    /// Return unique, sorted object keys resolved from all bundle `m_Container` entries that match `pattern`.
    pub fn bundle_container_root_keys(
        &self,
        pattern: &str,
        limit: Option<usize>,
    ) -> Vec<BinaryObjectKey> {
        let mut keys: Vec<BinaryObjectKey> = self
            .find_binary_object_keys_in_bundle_container(pattern)
            .into_iter()
            .map(|(_path, key)| key)
            .collect();
        keys.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        keys.dedup();
        if let Some(max) = limit {
            keys.truncate(max);
        }
        keys
    }
}

#[derive(Debug, Clone, Copy)]
enum GlobToken {
    Star,
    AnyChar,
    Literal(char),
}
