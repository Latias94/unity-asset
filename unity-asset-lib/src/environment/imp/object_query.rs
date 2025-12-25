use super::*;

impl Environment {
    /// Iterate binary object references across all loaded bundles and standalone serialized files.
    pub fn binary_object_infos(&self) -> impl Iterator<Item = BinaryObjectRef<'_>> {
        let typetree_options = self.options.typetree;
        let standalone_reporter = self.reporter.clone();
        let bundled_reporter = self.reporter.clone();

        let standalone = self.binary_assets.iter().flat_map(move |(source, file)| {
            let reporter = standalone_reporter.clone();
            file.object_handles().map(move |object| BinaryObjectRef {
                source,
                source_kind: BinarySourceKind::SerializedFile,
                asset_index: None,
                object,
                typetree_options,
                reporter: reporter.clone(),
            })
        });

        let bundled = self
            .bundles
            .iter()
            .flat_map(move |(bundle_source, bundle)| {
                let reporter = bundled_reporter.clone();
                bundle
                    .assets
                    .iter()
                    .enumerate()
                    .flat_map(move |(asset_index, file)| {
                        let reporter = reporter.clone();
                        file.object_handles().map(move |object| BinaryObjectRef {
                            source: bundle_source,
                            source_kind: BinarySourceKind::AssetBundle,
                            asset_index: Some(asset_index),
                            object,
                            typetree_options,
                            reporter: reporter.clone(),
                        })
                    })
            });

        standalone.chain(bundled)
    }

    /// List all loaded binary sources (standalone serialized files + bundles).
    pub fn binary_sources(&self) -> Vec<(BinarySourceKind, BinarySource)> {
        let mut out: Vec<(BinarySourceKind, BinarySource)> = Vec::new();

        let mut asset_sources: Vec<BinarySource> = self.binary_assets.keys().cloned().collect();
        asset_sources.sort();
        out.extend(
            asset_sources
                .into_iter()
                .map(|s| (BinarySourceKind::SerializedFile, s)),
        );

        let mut bundle_sources: Vec<BinarySource> = self.bundles.keys().cloned().collect();
        bundle_sources.sort();
        out.extend(
            bundle_sources
                .into_iter()
                .map(|s| (BinarySourceKind::AssetBundle, s)),
        );

        out
    }

    /// Find binary objects by `path_id` across all loaded assets/bundles.
    ///
    /// Note: `path_id` is unique within a single `SerializedFile`, but not globally unique across files.
    pub fn find_binary_objects(&self, path_id: i64) -> Vec<BinaryObjectRef<'_>> {
        let mut out = Vec::new();
        let typetree_options = self.options.typetree;
        let reporter = self.reporter.clone();

        let mut asset_sources: Vec<&BinarySource> = self.binary_assets.keys().collect();
        asset_sources.sort();
        for source in asset_sources {
            let file = &self.binary_assets[source];
            if let Some(object) = file.find_object_handle(path_id) {
                out.push(BinaryObjectRef {
                    source,
                    source_kind: BinarySourceKind::SerializedFile,
                    asset_index: None,
                    object,
                    typetree_options,
                    reporter: reporter.clone(),
                });
            }
        }

        let mut bundle_sources: Vec<&BinarySource> = self.bundles.keys().collect();
        bundle_sources.sort();
        for bundle_source in bundle_sources {
            let bundle = &self.bundles[bundle_source];
            for (asset_index, asset) in bundle.assets.iter().enumerate() {
                if let Some(object) = asset.find_object_handle(path_id) {
                    out.push(BinaryObjectRef {
                        source: bundle_source,
                        source_kind: BinarySourceKind::AssetBundle,
                        asset_index: Some(asset_index),
                        object,
                        typetree_options,
                        reporter: reporter.clone(),
                    });
                }
            }
        }

        out
    }

    /// Find the first matching binary object by `path_id` (best-effort).
    pub fn find_binary_object(&self, path_id: i64) -> Option<BinaryObjectRef<'_>> {
        self.find_binary_objects(path_id).into_iter().next()
    }

    /// Find binary objects by `path_id` within a specific loaded source (bundle path or `.assets` path).
    pub fn find_binary_objects_in_source<P: AsRef<Path>>(
        &self,
        source: P,
        path_id: i64,
    ) -> Vec<BinaryObjectRef<'_>> {
        let source = BinarySource::path(source.as_ref());
        self.find_binary_objects_in_source_id(&source, path_id)
    }

    /// Find binary objects by `path_id` within a specific loaded source (including WebFile entries).
    pub fn find_binary_objects_in_source_id(
        &self,
        source: &BinarySource,
        path_id: i64,
    ) -> Vec<BinaryObjectRef<'_>> {
        let typetree_options = self.options.typetree;
        let reporter = self.reporter.clone();

        if let Some((key, file)) = self.binary_assets.get_key_value(source) {
            if let Some(object) = file.find_object_handle(path_id) {
                return vec![BinaryObjectRef {
                    source: key,
                    source_kind: BinarySourceKind::SerializedFile,
                    asset_index: None,
                    object,
                    typetree_options,
                    reporter,
                }];
            }
            return Vec::new();
        }

        if let Some((key, bundle)) = self.bundles.get_key_value(source) {
            let mut out = Vec::new();
            for (asset_index, asset) in bundle.assets.iter().enumerate() {
                if let Some(object) = asset.find_object_handle(path_id) {
                    out.push(BinaryObjectRef {
                        source: key,
                        source_kind: BinarySourceKind::AssetBundle,
                        asset_index: Some(asset_index),
                        object,
                        typetree_options,
                        reporter: reporter.clone(),
                    });
                }
            }
            return out;
        }

        Vec::new()
    }

    /// Find the first binary object by `path_id` within a specific source.
    pub fn find_binary_object_in_source<P: AsRef<Path>>(
        &self,
        source: P,
        path_id: i64,
    ) -> Option<BinaryObjectRef<'_>> {
        self.find_binary_objects_in_source(source, path_id)
            .into_iter()
            .next()
    }

    pub fn find_binary_object_in_source_id(
        &self,
        source: &BinarySource,
        path_id: i64,
    ) -> Option<BinaryObjectRef<'_>> {
        self.find_binary_objects_in_source_id(source, path_id)
            .into_iter()
            .next()
    }

    /// Find a binary object by `path_id` within a specific bundle + asset index.
    pub fn find_binary_object_in_bundle_asset<P: AsRef<Path>>(
        &self,
        bundle_path: P,
        asset_index: usize,
        path_id: i64,
    ) -> Option<BinaryObjectRef<'_>> {
        let bundle_source = BinarySource::path(bundle_path.as_ref());
        self.find_binary_object_in_bundle_asset_source(&bundle_source, asset_index, path_id)
    }

    pub fn find_binary_object_in_bundle_asset_source(
        &self,
        bundle_source: &BinarySource,
        asset_index: usize,
        path_id: i64,
    ) -> Option<BinaryObjectRef<'_>> {
        let typetree_options = self.options.typetree;
        let reporter = self.reporter.clone();

        let (key, bundle) = self.bundles.get_key_value(bundle_source)?;
        let asset = bundle.assets.get(asset_index)?;
        let object = asset.find_object_handle(path_id)?;
        Some(BinaryObjectRef {
            source: key,
            source_kind: BinarySourceKind::AssetBundle,
            asset_index: Some(asset_index),
            object,
            typetree_options,
            reporter,
        })
    }

    /// Find globally-unique keys for all matching objects by `path_id` (best-effort).
    pub fn find_binary_object_keys(&self, path_id: i64) -> Vec<BinaryObjectKey> {
        self.find_binary_objects(path_id)
            .into_iter()
            .map(|r| r.key())
            .collect()
    }

    /// Find globally-unique keys for all matching objects by `path_id` within a specific source.
    pub fn find_binary_object_keys_in_source<P: AsRef<Path>>(
        &self,
        source: P,
        path_id: i64,
    ) -> Vec<BinaryObjectKey> {
        self.find_binary_objects_in_source(source, path_id)
            .into_iter()
            .map(|r| r.key())
            .collect()
    }

    /// Read a `UnityObject` from a globally-unique key.
    pub fn read_binary_object_key(&self, key: &BinaryObjectKey) -> Result<UnityObject> {
        let typetree_options = self.options.typetree;
        match key.source_kind {
            BinarySourceKind::SerializedFile => {
                let file = self.binary_assets.get(&key.source).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "SerializedFile source not loaded: {}",
                        key.source.describe()
                    ))
                })?;
                let object = file.find_object_handle(key.path_id).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Object not found in SerializedFile {}: path_id={}",
                        key.source.describe(),
                        key.path_id
                    ))
                })?;
                let obj = object.read_with_options(typetree_options).map_err(|e| {
                    UnityAssetError::with_source("Failed to parse binary object", e)
                })?;
                if let Some(reporter) = &self.reporter {
                    for w in obj.typetree_warnings() {
                        reporter.typetree_warning(key, w);
                    }
                }
                Ok(obj)
            }
            BinarySourceKind::AssetBundle => {
                let bundle = self.bundles.get(&key.source).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle source not loaded: {}",
                        key.source.describe()
                    ))
                })?;
                let asset_index = key.asset_index.ok_or_else(|| {
                    UnityAssetError::format(
                        "AssetBundle key requires an asset_index (which asset in the bundle?)"
                            .to_string(),
                    )
                })?;
                let file = bundle.assets.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset index out of range: {} asset_index={}",
                        key.source.describe(),
                        asset_index
                    ))
                })?;
                let object = file.find_object_handle(key.path_id).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Object not found in AssetBundle {} asset_index={}: path_id={}",
                        key.source.describe(),
                        asset_index,
                        key.path_id
                    ))
                })?;
                let obj = object.read_with_options(typetree_options).map_err(|e| {
                    UnityAssetError::with_source("Failed to parse binary object", e)
                })?;
                if let Some(reporter) = &self.reporter {
                    for w in obj.typetree_warnings() {
                        reporter.typetree_warning(key, w);
                    }
                }
                Ok(obj)
            }
        }
    }

    /// Best-effort peek of `m_Name`/`name` for a binary object key.
    ///
    /// This uses a TypeTree prefix fast path (when possible) and returns `Ok(None)` when the
    /// object has no TypeTree or does not expose a name field.
    pub fn peek_binary_object_name(&self, key: &BinaryObjectKey) -> Result<Option<String>> {
        let typetree_options = self.options.typetree;
        match key.source_kind {
            BinarySourceKind::SerializedFile => {
                let file = self.binary_assets.get(&key.source).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "SerializedFile source not loaded: {}",
                        key.source.describe()
                    ))
                })?;
                let object = file.find_object_handle(key.path_id).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Object not found in SerializedFile {}: path_id={}",
                        key.source.describe(),
                        key.path_id
                    ))
                })?;
                object
                    .peek_name_with_options(typetree_options)
                    .map_err(|e| {
                        UnityAssetError::with_source("Failed to peek binary object name", e)
                    })
            }
            BinarySourceKind::AssetBundle => {
                let bundle = self.bundles.get(&key.source).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle source not loaded: {}",
                        key.source.describe()
                    ))
                })?;
                let asset_index = key.asset_index.ok_or_else(|| {
                    UnityAssetError::format(
                        "AssetBundle key requires an asset_index (which asset in the bundle?)"
                            .to_string(),
                    )
                })?;
                let file = bundle.assets.get(asset_index).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset index out of range: {} asset_index={}",
                        key.source.describe(),
                        asset_index
                    ))
                })?;
                let object = file.find_object_handle(key.path_id).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "Object not found in AssetBundle {} asset_index={}: path_id={}",
                        key.source.describe(),
                        asset_index,
                        key.path_id
                    ))
                })?;
                object
                    .peek_name_with_options(typetree_options)
                    .map_err(|e| {
                        UnityAssetError::with_source("Failed to peek binary object name", e)
                    })
            }
        }
    }
}
