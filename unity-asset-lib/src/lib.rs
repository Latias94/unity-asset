//! Unity Asset Parser
//!
//! A comprehensive Rust library for parsing Unity asset files, supporting both YAML and binary formats.
//!
//! This crate provides high-performance, memory-safe parsing of Unity files
//! while aiming for best-effort compatibility with Unity's formats (correctness and coverage are ongoing work).
//!
//! # Features
//!
//! - **YAML Processing**: Complete Unity YAML format support with multi-document parsing
//! - **Binary Assets**: AssetBundle and SerializedFile parsing with compression support
//! - **Async Support**: Optional async/await API for concurrent processing (enable with `async` feature)
//! - **Type Safety**: Rust's type system prevents common parsing vulnerabilities
//! - **Performance**: Designed for reasonable performance; some workflows may be eager by default
//!
//! # Examples
//!
//! ## Basic YAML Processing
//!
//! ```rust,no_run
//! use unity_asset::{YamlDocument, UnityDocument};
//!
//! // Load a Unity YAML file
//! let doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;
//!
//! // Access and filter objects
//! let settings = doc.get(Some("PlayerSettings"), None)?;
//! println!("Product name: {:?}", settings.get("productName"));
//!
//! # Ok::<(), unity_asset::UnityAssetError>(())
//! ```
//!
//! ## Binary Asset Processing
//!
//! ```rust,no_run
//! use unity_asset::load_bundle_from_memory;
//!
//! // Load and parse AssetBundle
//! let data = std::fs::read("game.bundle")?;
//! let bundle = load_bundle_from_memory(data)?;
//!
//! // Process assets
//! for asset in &bundle.assets {
//!     println!("Found asset with {} objects", asset.object_count());
//! }
//!
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Async Processing (requires `async` feature)
//!
//! ```rust,no_run
//! # #[cfg(feature = "async")]
//! # {
//! use unity_asset::{YamlDocument, AsyncUnityDocument};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Load file asynchronously
//!     let doc = YamlDocument::load_yaml_async("ProjectSettings.asset", false).await?;
//!
//!     // Same API as sync version
//!     let settings = doc.get(Some("PlayerSettings"), None)?;
//!     println!("Product name: {:?}", settings.get("productName"));
//!
//!     Ok(())
//! }
//! # }
//! ```

// Re-export from core crate
pub use unity_asset_core::{
    DocumentFormat, Result, UnityAssetError, UnityClass, UnityClassRegistry, UnityDocument,
    UnityValue, constants::*,
};

pub use unity_asset_core::get_class_name;

// Re-export from YAML crate
pub use unity_asset_yaml::YamlDocument;

// Re-export from binary crate
pub use unity_asset_binary::{
    AssetBundle, SerializedFile, load_bundle, load_bundle_from_memory, load_bundle_with_options,
};

// Re-export async traits when async feature is enabled
#[cfg(feature = "async")]
pub use unity_asset_core::document::AsyncUnityDocument;

/// Environment for managing multiple Unity assets
pub mod environment {
    use crate::{Result, YamlDocument};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use unity_asset_binary::{AssetBundle, ObjectHandle, SerializedFile, UnityObject};
    use unity_asset_core::UnityValue;
    use unity_asset_core::{UnityAssetError, UnityClass, UnityDocument};

    /// A reference to a binary object within a `SerializedFile`.
    ///
    /// This is conceptually similar to UnityPy's `ObjectReader`: it is a lightweight handle that can be
    /// converted into a parsed `UnityObject` on-demand.
    #[derive(Debug, Clone, Copy)]
    pub struct BinaryObjectRef<'a> {
        pub source_path: &'a PathBuf,
        pub source_kind: BinarySourceKind,
        /// Asset index within a bundle. `None` for standalone serialized files.
        pub asset_index: Option<usize>,
        pub object: ObjectHandle<'a>,
    }

    impl<'a> BinaryObjectRef<'a> {
        pub fn read(&self) -> Result<UnityObject> {
            self.object.read().map_err(|e| {
                UnityAssetError::format(format!("Failed to parse binary object: {}", e))
            })
        }

        /// Create a globally-unique key for this object reference.
        pub fn key(&self) -> BinaryObjectKey {
            BinaryObjectKey {
                source_path: self.source_path.clone(),
                source_kind: self.source_kind,
                asset_index: self.asset_index,
                path_id: self.object.path_id(),
            }
        }
    }

    /// A unified object reference across YAML and binary formats.
    #[derive(Debug, Clone, Copy)]
    pub enum EnvironmentObjectRef<'a> {
        Yaml(&'a UnityClass),
        Binary(BinaryObjectRef<'a>),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum BinarySourceKind {
        SerializedFile,
        AssetBundle,
    }

    /// A globally-unique identifier for a binary object.
    ///
    /// `path_id` is only unique within a single `SerializedFile`, so we include a source path
    /// (bundle/asset path) and optional bundle asset index.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct BinaryObjectKey {
        pub source_path: PathBuf,
        pub source_kind: BinarySourceKind,
        pub asset_index: Option<usize>,
        pub path_id: i64,
    }

    impl std::fmt::Display for BinaryObjectKey {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // A copy/paste friendly key format that can be parsed back with `FromStr`.
            //
            // Format:
            //   bok1|<kind>|<asset_index_or_dash>|<path_id>|<path_utf8_len>|<path>
            //
            // The final `<path>` field can contain any characters (including `|`) because it is the
            // last segment and is length-validated during parsing.
            let kind = match self.source_kind {
                BinarySourceKind::SerializedFile => "serialized",
                BinarySourceKind::AssetBundle => "bundle",
            };
            let asset_index = self
                .asset_index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".to_string());
            let path = self.source_path.to_string_lossy();
            write!(
                f,
                "bok1|{}|{}|{}|{}|{}",
                kind,
                asset_index,
                self.path_id,
                path.as_bytes().len(),
                path
            )
        }
    }

    impl FromStr for BinaryObjectKey {
        type Err = String;

        fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
            let prefix = "bok1|";
            if !s.starts_with(prefix) {
                return Err("invalid key prefix (expected: bok1|...)".to_string());
            }

            let mut rest = &s[prefix.len()..];
            let (kind, r) = split_once(rest, '|').ok_or_else(|| "missing kind".to_string())?;
            rest = r;
            let (asset_index, r) =
                split_once(rest, '|').ok_or_else(|| "missing asset_index".to_string())?;
            rest = r;
            let (path_id, r) =
                split_once(rest, '|').ok_or_else(|| "missing path_id".to_string())?;
            rest = r;
            let (path_len, path) =
                split_once(rest, '|').ok_or_else(|| "missing path_len/path".to_string())?;

            let source_kind = match kind {
                "bundle" => BinarySourceKind::AssetBundle,
                "serialized" => BinarySourceKind::SerializedFile,
                other => return Err(format!("unknown kind: {}", other)),
            };

            let asset_index = if asset_index == "-" || asset_index.is_empty() {
                None
            } else {
                Some(
                    asset_index
                        .parse::<usize>()
                        .map_err(|e| format!("invalid asset_index: {}", e))?,
                )
            };

            let path_id = path_id
                .parse::<i64>()
                .map_err(|e| format!("invalid path_id: {}", e))?;

            let expected_len = path_len
                .parse::<usize>()
                .map_err(|e| format!("invalid path_len: {}", e))?;
            if path.as_bytes().len() != expected_len {
                return Err(format!(
                    "path length mismatch: expected {} bytes, got {} bytes",
                    expected_len,
                    path.as_bytes().len()
                ));
            }

            if source_kind == BinarySourceKind::AssetBundle && asset_index.is_none() {
                return Err("asset_index is required for bundle keys".to_string());
            }

            Ok(Self {
                source_path: PathBuf::from(path),
                source_kind,
                asset_index,
                path_id,
            })
        }
    }

    fn split_once<'a>(s: &'a str, delim: char) -> Option<(&'a str, &'a str)> {
        let pos = s.find(delim)?;
        Some((&s[..pos], &s[pos + delim.len_utf8()..]))
    }

    /// A best-effort entry extracted from an AssetBundle `m_Container`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BundleContainerEntry {
        pub bundle_path: PathBuf,
        pub asset_index: usize,
        pub asset_path: String,
        pub file_id: i32,
        pub path_id: i64,
        pub key: Option<BinaryObjectKey>,
    }

    /// Unified environment for managing Unity assets
    pub struct Environment {
        /// Loaded YAML documents
        yaml_documents: HashMap<PathBuf, YamlDocument>,
        /// Loaded standalone SerializedFiles (e.g. `.assets`)
        binary_assets: HashMap<PathBuf, SerializedFile>,
        /// Loaded AssetBundles (e.g. `.bundle`, `.unity3d`, `.ab`)
        bundles: HashMap<PathBuf, AssetBundle>,
        bundle_container_cache: RefCell<HashMap<PathBuf, Vec<BundleContainerEntry>>>,
        /// Base path for relative file resolution
        #[allow(dead_code)]
        base_path: PathBuf,
    }

    impl Environment {
        /// Create a new environment
        pub fn new() -> Self {
            Self {
                yaml_documents: HashMap::new(),
                binary_assets: HashMap::new(),
                bundles: HashMap::new(),
                bundle_container_cache: RefCell::new(HashMap::new()),
                base_path: std::env::current_dir().unwrap_or_default(),
            }
        }

        /// Load assets from a path (file or directory)
        pub fn load<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
            let path = path.as_ref();

            if path.is_file() {
                self.load_file(path)?;
            } else if path.is_dir() {
                self.load_directory(path)?;
            }

            Ok(())
        }

        /// Load a single file
        pub fn load_file<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
            let path = path.as_ref();

            // Check file extension to determine type
            if let Some(ext) = path.extension() {
                match ext.to_str() {
                    Some("asset") | Some("prefab") | Some("unity") | Some("meta") => {
                        match YamlDocument::load_yaml(path, false) {
                            Ok(doc) => {
                                self.yaml_documents.insert(path.to_path_buf(), doc);
                            }
                            Err(_) => {
                                // Some Unity projects can store `.asset`-like files in binary form.
                                // If YAML parsing fails, fall back to binary detection.
                                self.try_load_binary(path)?;
                            }
                        }
                    }
                    Some("assets") => {
                        self.try_load_serialized_file(path)?;
                    }
                    Some("bundle") | Some("unity3d") | Some("ab") => {
                        self.try_load_bundle(path)?;
                    }
                    _ => {
                        // Best-effort binary detection for common build outputs.
                        self.try_load_binary(path)?;
                    }
                }
            }

            Ok(())
        }

        fn try_load_binary(&mut self, path: &Path) -> Result<()> {
            // Try bundle first, then serialized file.
            if self.try_load_bundle(path).is_ok() {
                return Ok(());
            }
            if self.try_load_serialized_file(path).is_ok() {
                return Ok(());
            }
            Ok(())
        }

        fn try_load_bundle(&mut self, path: &Path) -> Result<()> {
            let data = std::fs::read(path).map_err(|e| {
                UnityAssetError::format(format!("Failed to read file {:?}: {}", path, e))
            })?;
            match unity_asset_binary::load_bundle_from_memory(data) {
                Ok(bundle) => {
                    self.bundles.insert(path.to_path_buf(), bundle);
                    self.bundle_container_cache.borrow_mut().remove(path);
                    Ok(())
                }
                Err(e) => Err(UnityAssetError::format(format!(
                    "Failed to parse AssetBundle {:?}: {}",
                    path, e
                ))),
            }
        }

        fn try_load_serialized_file(&mut self, path: &Path) -> Result<()> {
            let data = std::fs::read(path).map_err(|e| {
                UnityAssetError::format(format!("Failed to read file {:?}: {}", path, e))
            })?;
            match unity_asset_binary::parse_serialized_file(data) {
                Ok(asset) => {
                    self.binary_assets.insert(path.to_path_buf(), asset);
                    self.bundle_container_cache.borrow_mut().clear();
                    Ok(())
                }
                Err(e) => Err(UnityAssetError::format(format!(
                    "Failed to parse SerializedFile {:?}: {}",
                    path, e
                ))),
            }
        }

        /// Load all supported files from a directory
        pub fn load_directory<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
            let path = path.as_ref();

            if !path.exists() {
                return Err(UnityAssetError::format(format!(
                    "Directory does not exist: {:?}",
                    path
                )));
            }

            if !path.is_dir() {
                return Err(UnityAssetError::format(format!(
                    "Path is not a directory: {:?}",
                    path
                )));
            }

            // Recursively traverse directory
            self.traverse_directory(path)?;

            Ok(())
        }

        /// Recursively traverse directory and load Unity files
        fn traverse_directory(&mut self, dir: &Path) -> Result<()> {
            let entries = std::fs::read_dir(dir).map_err(|e| {
                UnityAssetError::format(format!("Failed to read directory {:?}: {}", dir, e))
            })?;

            for entry in entries {
                let entry = entry.map_err(|e| {
                    UnityAssetError::format(format!("Failed to read directory entry: {}", e))
                })?;
                let path = entry.path();

                if path.is_dir() {
                    // Skip common Unity directories that don't contain assets
                    if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                        match dir_name {
                            "Library" | "Temp" | "Logs" | ".git" | ".vs" | "obj" | "bin" => {
                                continue; // Skip these directories
                            }
                            _ => {
                                // Recursively process subdirectory
                                self.traverse_directory(&path)?;
                            }
                        }
                    }
                } else if path.is_file() {
                    // Try to load the file
                    if let Err(e) = self.load_file(&path) {
                        // Log error but continue processing other files
                        eprintln!("Warning: Failed to load {:?}: {}", path, e);
                    }
                }
            }

            Ok(())
        }

        /// Iterate YAML Unity objects.
        pub fn yaml_objects(&self) -> impl Iterator<Item = &UnityClass> {
            self.yaml_documents.values().flat_map(|doc| doc.entries())
        }

        /// Find a YAML object by its YAML anchor (the `&<id>` part).
        pub fn find_yaml_by_anchor(&self, anchor: &str) -> Option<&UnityClass> {
            self.yaml_objects().find(|obj| obj.anchor == anchor)
        }

        /// Iterate binary object references across all loaded bundles and standalone serialized files.
        pub fn binary_object_infos(&self) -> impl Iterator<Item = BinaryObjectRef<'_>> {
            let standalone = self.binary_assets.iter().flat_map(|(path, file)| {
                file.object_handles().map(move |object| BinaryObjectRef {
                    source_path: path,
                    source_kind: BinarySourceKind::SerializedFile,
                    asset_index: None,
                    object,
                })
            });

            let bundled = self.bundles.iter().flat_map(|(bundle_path, bundle)| {
                bundle
                    .assets
                    .iter()
                    .enumerate()
                    .flat_map(move |(asset_index, file)| {
                        file.object_handles().map(move |object| BinaryObjectRef {
                            source_path: bundle_path,
                            source_kind: BinarySourceKind::AssetBundle,
                            asset_index: Some(asset_index),
                            object,
                        })
                    })
            });

            standalone.chain(bundled)
        }

        /// List all loaded binary sources (standalone serialized files + bundles).
        pub fn binary_sources(&self) -> Vec<(BinarySourceKind, &PathBuf)> {
            let mut out: Vec<(BinarySourceKind, &PathBuf)> = Vec::new();

            let mut asset_paths: Vec<&PathBuf> = self.binary_assets.keys().collect();
            asset_paths.sort();
            out.extend(
                asset_paths
                    .into_iter()
                    .map(|p| (BinarySourceKind::SerializedFile, p)),
            );

            let mut bundle_paths: Vec<&PathBuf> = self.bundles.keys().collect();
            bundle_paths.sort();
            out.extend(
                bundle_paths
                    .into_iter()
                    .map(|p| (BinarySourceKind::AssetBundle, p)),
            );

            out
        }

        /// Find binary objects by `path_id` across all loaded assets/bundles.
        ///
        /// Note: `path_id` is unique within a single `SerializedFile`, but not globally unique across files.
        pub fn find_binary_objects(&self, path_id: i64) -> Vec<BinaryObjectRef<'_>> {
            let mut out = Vec::new();

            let mut asset_paths: Vec<&PathBuf> = self.binary_assets.keys().collect();
            asset_paths.sort();
            for path in asset_paths {
                let file = &self.binary_assets[path];
                if let Some(object) = file.find_object_handle(path_id) {
                    out.push(BinaryObjectRef {
                        source_path: path,
                        source_kind: BinarySourceKind::SerializedFile,
                        asset_index: None,
                        object,
                    });
                }
            }

            let mut bundle_paths: Vec<&PathBuf> = self.bundles.keys().collect();
            bundle_paths.sort();
            for bundle_path in bundle_paths {
                let bundle = &self.bundles[bundle_path];
                for (asset_index, asset) in bundle.assets.iter().enumerate() {
                    if let Some(object) = asset.find_object_handle(path_id) {
                        out.push(BinaryObjectRef {
                            source_path: bundle_path,
                            source_kind: BinarySourceKind::AssetBundle,
                            asset_index: Some(asset_index),
                            object,
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
            let source = source.as_ref();

            if let Some((key, file)) = self.binary_assets.get_key_value(source) {
                if let Some(object) = file.find_object_handle(path_id) {
                    return vec![BinaryObjectRef {
                        source_path: key,
                        source_kind: BinarySourceKind::SerializedFile,
                        asset_index: None,
                        object,
                    }];
                }
                return Vec::new();
            }

            if let Some((key, bundle)) = self.bundles.get_key_value(source) {
                let mut out = Vec::new();
                for (asset_index, asset) in bundle.assets.iter().enumerate() {
                    if let Some(object) = asset.find_object_handle(path_id) {
                        out.push(BinaryObjectRef {
                            source_path: key,
                            source_kind: BinarySourceKind::AssetBundle,
                            asset_index: Some(asset_index),
                            object,
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

        /// Find a binary object by `path_id` within a specific bundle + asset index.
        pub fn find_binary_object_in_bundle_asset<P: AsRef<Path>>(
            &self,
            bundle_path: P,
            asset_index: usize,
            path_id: i64,
        ) -> Option<BinaryObjectRef<'_>> {
            let bundle_path = bundle_path.as_ref();
            let (key, bundle) = self.bundles.get_key_value(bundle_path)?;
            let asset = bundle.assets.get(asset_index)?;
            let object = asset.find_object_handle(path_id)?;
            Some(BinaryObjectRef {
                source_path: key,
                source_kind: BinarySourceKind::AssetBundle,
                asset_index: Some(asset_index),
                object,
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
            match key.source_kind {
                BinarySourceKind::SerializedFile => {
                    let file = self.binary_assets.get(&key.source_path).ok_or_else(|| {
                        UnityAssetError::format(format!(
                            "SerializedFile source not loaded: {:?}",
                            key.source_path
                        ))
                    })?;
                    let object = file.find_object_handle(key.path_id).ok_or_else(|| {
                        UnityAssetError::format(format!(
                            "Object not found in SerializedFile {:?}: path_id={}",
                            key.source_path, key.path_id
                        ))
                    })?;
                    object.read().map_err(|e| {
                        UnityAssetError::format(format!("Failed to parse binary object: {}", e))
                    })
                }
                BinarySourceKind::AssetBundle => {
                    let bundle = self.bundles.get(&key.source_path).ok_or_else(|| {
                        UnityAssetError::format(format!(
                            "AssetBundle source not loaded: {:?}",
                            key.source_path
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
                            "AssetBundle asset index out of range: {:?} asset_index={}",
                            key.source_path, asset_index
                        ))
                    })?;
                    let object = file.find_object_handle(key.path_id).ok_or_else(|| {
                        UnityAssetError::format(format!(
                            "Object not found in AssetBundle {:?} asset_index={}: path_id={}",
                            key.source_path, asset_index, key.path_id
                        ))
                    })?;
                    object.read().map_err(|e| {
                        UnityAssetError::format(format!("Failed to parse binary object: {}", e))
                    })
                }
            }
        }

        fn find_loaded_serialized_file_by_external_path(
            &self,
            external_path: &str,
        ) -> Option<PathBuf> {
            if external_path.is_empty() {
                return None;
            }

            let direct = Path::new(external_path);
            if self.binary_assets.contains_key(direct) {
                return Some(direct.to_path_buf());
            }

            if !direct.is_absolute() {
                let joined = self.base_path.join(direct);
                if self.binary_assets.contains_key(&joined) {
                    return Some(joined);
                }
            }

            let target_file_name = direct.file_name().and_then(|n| n.to_str());
            let mut by_name: Vec<&PathBuf> = Vec::new();
            if let Some(name) = target_file_name {
                by_name.extend(
                    self.binary_assets
                        .keys()
                        .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(name)),
                );
            }
            by_name.sort();
            if let Some(found) = by_name.first() {
                return Some((*found).clone());
            }

            let external_norm = external_path.replace('\\', "/");
            let mut by_suffix: Vec<&PathBuf> = self
                .binary_assets
                .keys()
                .filter(|p| {
                    let p_str = p.to_string_lossy().replace('\\', "/");
                    p_str.ends_with(&external_norm) || external_norm.ends_with(&p_str)
                })
                .collect();
            by_suffix.sort();
            by_suffix.first().cloned().cloned()
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
                    source_path: context.source_path.clone(),
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
                if let Some(bundle) = self.bundles.get(context.source_path) {
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
                        if name_norm.ends_with(&external_norm)
                            || external_norm.ends_with(&name_norm)
                        {
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
                            source_path: context.source_path.clone(),
                            source_kind: BinarySourceKind::AssetBundle,
                            asset_index: Some(asset_index),
                            path_id,
                        });
                    }
                }
            }

            // Fallback: resolve to an already-loaded standalone serialized file on disk.
            let resolved_source =
                self.find_loaded_serialized_file_by_external_path(&external.path)?;
            Some(BinaryObjectKey {
                source_path: resolved_source,
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
                if path_id == 0 {
                    continue;
                }

                let key = self.resolve_binary_pptr(context, file_id, path_id);
                out.push(BundleContainerEntry {
                    bundle_path: context.source_path.clone(),
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
            let bundle_path = bundle_path.as_ref();
            if let Some(cached) = self.bundle_container_cache.borrow().get(bundle_path) {
                return Ok(cached.clone());
            }

            let (key, bundle) = self.bundles.get_key_value(bundle_path).ok_or_else(|| {
                UnityAssetError::format(format!("AssetBundle source not loaded: {:?}", bundle_path))
            })?;

            let mut out: Vec<BundleContainerEntry> = Vec::new();

            for (asset_index, file) in bundle.assets.iter().enumerate() {
                for object in file.object_handles() {
                    if object.class_id() != 142 {
                        continue;
                    }
                    let obj_ref = BinaryObjectRef {
                        source_path: key,
                        source_kind: BinarySourceKind::AssetBundle,
                        asset_index: Some(asset_index),
                        object,
                    };

                    // First, try TypeTree extraction when available.
                    if object.file().enable_type_tree {
                        if let Ok(parsed) = obj_ref.read() {
                            let extracted =
                                self.extract_assetbundle_container_from_typetree(&obj_ref, &parsed);
                            if !extracted.is_empty() {
                                out.extend(extracted);
                                continue;
                            }
                        }
                    }

                    // Fallback: raw parsing for stripped TypeTree bundles.
                    if let Ok(raw_entries) = object.file().assetbundle_container_raw(object.info())
                    {
                        for (asset_path, file_id, path_id) in raw_entries {
                            if path_id == 0 {
                                continue;
                            }
                            let key = self
                                .resolve_binary_pptr(&obj_ref, file_id, path_id)
                                .or_else(|| {
                                    // Fallback: if external mapping fails, try to locate the object by `path_id`
                                    // within the same bundle. This is best-effort and only used when `file_id`
                                    // can't be resolved.
                                    let matches = self.find_binary_objects_in_source(
                                        obj_ref.source_path,
                                        path_id,
                                    );
                                    if matches.len() == 1 {
                                        Some(matches[0].key())
                                    } else {
                                        None
                                    }
                                });
                            out.push(BundleContainerEntry {
                                bundle_path: obj_ref.source_path.clone(),
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

            self.bundle_container_cache
                .borrow_mut()
                .insert(bundle_path.to_path_buf(), out.clone());
            Ok(out)
        }

        /// Find container entries across all loaded bundles whose `asset_path` contains `pattern`.
        pub fn find_bundle_container_entries(&self, pattern: &str) -> Vec<BundleContainerEntry> {
            let mut bundle_paths: Vec<&PathBuf> = self.bundles.keys().collect();
            bundle_paths.sort();

            let mut out = Vec::new();
            for bundle_path in bundle_paths {
                if let Ok(entries) = self.bundle_container_entries(bundle_path) {
                    out.extend(
                        entries
                            .into_iter()
                            .filter(|e| e.asset_path.contains(pattern)),
                    );
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

        /// Iterate all objects (YAML + binary) as lightweight references.
        pub fn objects(&self) -> Box<dyn Iterator<Item = EnvironmentObjectRef<'_>> + '_> {
            let yaml_iter = self.yaml_objects().map(EnvironmentObjectRef::Yaml);
            let bin_iter = self.binary_object_infos().map(EnvironmentObjectRef::Binary);
            Box::new(yaml_iter.chain(bin_iter))
        }

        /// Iterate parsed binary `UnityObject`s (best-effort).
        pub fn binary_objects(&self) -> impl Iterator<Item = Result<UnityObject>> + '_ {
            self.binary_object_infos().map(|r| r.read())
        }

        /// Filter YAML objects by class name.
        pub fn filter_by_class(&self, class_name: &str) -> Vec<&UnityClass> {
            self.yaml_objects()
                .filter(|obj| obj.class_name == class_name)
                .collect()
        }

        /// Get loaded YAML documents
        pub fn yaml_documents(&self) -> &HashMap<PathBuf, YamlDocument> {
            &self.yaml_documents
        }

        /// Get loaded standalone SerializedFiles.
        pub fn binary_assets(&self) -> &HashMap<PathBuf, SerializedFile> {
            &self.binary_assets
        }

        /// Get loaded AssetBundles.
        pub fn bundles(&self) -> &HashMap<PathBuf, AssetBundle> {
            &self.bundles
        }

        fn normalize_stream_path(stream_path: &str) -> String {
            let mut p = stream_path.trim().to_string();
            if let Some(rest) = p.strip_prefix("archive:/") {
                p = rest.to_string();
            }
            p = p.replace('\\', "/");
            while p.starts_with("./") {
                p = p.trim_start_matches("./").to_string();
            }
            p
        }

        fn cab_prefix_from_normalized(normalized: &str) -> Option<String> {
            let needle = "CAB-";
            let start = normalized.find(needle)?;
            let mut hex = String::with_capacity(32);
            for ch in normalized[start + needle.len()..].chars() {
                if ch.is_ascii_hexdigit() && hex.len() < 32 {
                    hex.push(ch);
                } else {
                    break;
                }
            }
            if hex.len() == 32 {
                Some(format!("CAB-{}", hex))
            } else {
                None
            }
        }

        fn find_bundle_resource_node<'a>(
            bundle: &'a AssetBundle,
            stream_path: &str,
        ) -> Option<&'a unity_asset_binary::bundle::types::DirectoryNode> {
            let normalized = Self::normalize_stream_path(stream_path);
            if normalized.is_empty() {
                return None;
            }

            let file_name = Path::new(&normalized)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());

            let mut nodes: Vec<&unity_asset_binary::bundle::types::DirectoryNode> =
                bundle.nodes.iter().filter(|n| n.is_file()).collect();
            nodes.sort_by(|a, b| a.name.cmp(&b.name));

            for node in &nodes {
                let node_norm = node.name.replace('\\', "/");
                if node_norm == normalized
                    || node_norm.ends_with(&normalized)
                    || normalized.ends_with(&node_norm)
                {
                    return Some(*node);
                }

                if let Some(file_name) = &file_name {
                    if Path::new(&node_norm).file_name().and_then(|n| n.to_str())
                        == Some(file_name.as_str())
                    {
                        return Some(*node);
                    }
                }
            }

            // Unity sometimes appends an index suffix to the CAB resource node name
            // (e.g. `CAB-<hash>1.resource`) while the `StreamedResource.m_Source` path
            // points to `CAB-<hash>.resource`. Best-effort: match by CAB prefix.
            let cab_prefix = normalized
                .split('/')
                .find(|s| s.starts_with("CAB-"))
                .and_then(|s| {
                    let hash: String = s
                        .trim_start_matches("CAB-")
                        .chars()
                        .take_while(|c| c.is_ascii_hexdigit())
                        .collect();
                    if hash.is_empty() {
                        None
                    } else {
                        Some(format!("CAB-{}", hash))
                    }
                });

            if let Some(cab_prefix) = cab_prefix {
                for node in &nodes {
                    let node_norm = node.name.replace('\\', "/");
                    let is_resource =
                        node_norm.ends_with(".resS") || node_norm.ends_with(".resource");
                    let base = Path::new(&node_norm)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&node_norm);
                    if is_resource
                        && (node_norm.starts_with(&cab_prefix) || base.starts_with(&cab_prefix))
                    {
                        return Some(*node);
                    }
                }
            }

            None
        }

        fn stream_fs_candidates(source_path: &Path, stream_path: &str) -> Vec<PathBuf> {
            let base_dir = source_path.parent().unwrap_or_else(|| Path::new("."));
            let normalized = Self::normalize_stream_path(stream_path);
            let cab_prefix = Self::cab_prefix_from_normalized(&normalized);

            let mut dirs = vec![base_dir.to_path_buf(), base_dir.join("StreamingAssets")];
            if let Some(cab) = &cab_prefix {
                dirs.push(base_dir.join(cab));
                dirs.push(base_dir.join("StreamingAssets").join(cab));
            }
            dirs.sort();
            dirs.dedup();

            let mut candidates: Vec<PathBuf> = Vec::new();

            // If the path already exists as-is (e.g. absolute path), try it first.
            candidates.push(PathBuf::from(stream_path));

            if !normalized.is_empty() {
                candidates.push(base_dir.join(&normalized));
                if let Some(file_name) = Path::new(&normalized).file_name() {
                    candidates.push(base_dir.join(file_name));
                    candidates.push(base_dir.join("StreamingAssets").join(file_name));
                }
            }

            // Unity often stores resources as `CAB-<hash><n>.resource` / `.resS` on disk,
            // while the stream path references `CAB-<hash>.resource` (no suffix).
            if let Some(cab) = &cab_prefix {
                for ext in ["resource", "resS"] {
                    for dir in &dirs {
                        candidates.push(dir.join(format!("{cab}.{ext}")));
                    }
                    for suffix in 1..=9 {
                        for dir in &dirs {
                            candidates.push(dir.join(format!("{cab}{suffix}.{ext}")));
                        }
                    }
                }

                // Targeted directory scans (non-recursive) to catch suffixes beyond 9.
                for dir in &dirs {
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                                continue;
                            };
                            if !(name.ends_with(".resS") || name.ends_with(".resource")) {
                                continue;
                            }
                            if name.starts_with(cab) {
                                candidates.push(path);
                            }
                        }
                    }
                }
            }

            candidates.sort();
            candidates.dedup();
            candidates
        }

        /// Read streamed resource bytes from a loaded bundle.
        ///
        /// This is primarily used for `AudioClip` / `Texture2D` stream data (`m_StreamData`) when the
        /// referenced resource file is contained inside the same bundle (e.g. `.resS` / `.resource`).
        pub fn read_bundle_stream_data<P: AsRef<Path>>(
            &self,
            bundle_path: P,
            stream_path: &str,
            offset: u64,
            size: u32,
        ) -> Result<Vec<u8>> {
            let bundle_path = bundle_path.as_ref();
            let bundle = self.bundles.get(bundle_path).ok_or_else(|| {
                UnityAssetError::format(format!("AssetBundle source not loaded: {:?}", bundle_path))
            })?;

            let node = Self::find_bundle_resource_node(bundle, stream_path).ok_or_else(|| {
                UnityAssetError::format(format!(
                    "Resource node not found in bundle {:?}: {}",
                    bundle_path, stream_path
                ))
            })?;

            let node_start: usize = node.offset.try_into().map_err(|_| {
                UnityAssetError::format(format!("Resource node offset overflow: {}", node.offset))
            })?;
            let node_size: usize = node.size.try_into().map_err(|_| {
                UnityAssetError::format(format!("Resource node size overflow: {}", node.size))
            })?;
            let data = bundle.data();
            if node_start.saturating_add(node_size) > data.len() {
                return Err(UnityAssetError::format(format!(
                    "Resource node out of bounds: name={}, offset={}, size={}, bundle_len={}",
                    node.name,
                    node.offset,
                    node.size,
                    data.len()
                )));
            }

            let offset_usize: usize = offset.try_into().map_err(|_| {
                UnityAssetError::format(format!("Stream offset overflow: {}", offset))
            })?;
            let size_usize: usize = size
                .try_into()
                .map_err(|_| UnityAssetError::format(format!("Stream size overflow: {}", size)))?;

            if offset_usize.saturating_add(size_usize) > node_size {
                return Err(UnityAssetError::format(format!(
                    "Stream range out of bounds: name={}, stream_offset={}, stream_size={}, node_size={}",
                    node.name, offset, size, node.size
                )));
            }

            let start = node_start.saturating_add(offset_usize);
            let end = start.saturating_add(size_usize);
            Ok(data[start..end].to_vec())
        }

        /// Read streamed resource bytes (best-effort) using the current environment context.
        ///
        /// Resolution strategy:
        /// - If `source_kind` is `AssetBundle`, try to read from resource nodes inside the same bundle.
        /// - Fall back to reading from the filesystem (same directory / `StreamingAssets/`), which
        ///   matches UnityPy's `ResourceReader`-like behavior.
        pub fn read_stream_data<P: AsRef<Path>>(
            &self,
            source_path: P,
            source_kind: BinarySourceKind,
            stream_path: &str,
            offset: u64,
            size: u32,
        ) -> Result<Vec<u8>> {
            let source_path = source_path.as_ref();
            if size == 0 {
                return Ok(Vec::new());
            }

            match source_kind {
                BinarySourceKind::AssetBundle => self
                    .read_bundle_stream_data(source_path, stream_path, offset, size)
                    .or_else(|_| {
                        self.read_stream_data_from_fs(source_path, stream_path, offset, size)
                    }),
                BinarySourceKind::SerializedFile => {
                    self.read_stream_data_from_fs(source_path, stream_path, offset, size)
                }
            }
        }

        /// Read streamed resource bytes from the filesystem (best-effort).
        ///
        /// This is useful when `StreamedResource.m_Source` points to an external `.resS`/`.resource`
        /// file that is not embedded in the current bundle.
        pub fn read_stream_data_from_fs<P: AsRef<Path>>(
            &self,
            source_path: P,
            stream_path: &str,
            offset: u64,
            size: u32,
        ) -> Result<Vec<u8>> {
            use std::fs::File;
            use std::io::{Read, Seek, SeekFrom};

            let source_path = source_path.as_ref();
            let candidates = Self::stream_fs_candidates(source_path, stream_path);
            for candidate in candidates {
                if !candidate.exists() {
                    continue;
                }
                let mut file = File::open(&candidate).map_err(|e| {
                    UnityAssetError::format(format!(
                        "Failed to open stream resource {:?}: {}",
                        candidate, e
                    ))
                })?;
                file.seek(SeekFrom::Start(offset)).map_err(|e| {
                    UnityAssetError::format(format!(
                        "Failed to seek stream resource {:?} to {}: {}",
                        candidate, offset, e
                    ))
                })?;

                let mut buffer = vec![0u8; size as usize];
                file.read_exact(&mut buffer).map_err(|e| {
                    UnityAssetError::format(format!(
                        "Failed to read stream resource {:?} (offset={}, size={}): {}",
                        candidate, offset, size, e
                    ))
                })?;
                return Ok(buffer);
            }

            Err(UnityAssetError::format(format!(
                "Stream resource file not found for source {:?}: {}",
                source_path, stream_path
            )))
        }
    }

    impl Default for Environment {
        fn default() -> Self {
            Self::new()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;

        #[test]
        fn environment_loads_yaml_fixture() {
            let mut env = Environment::new();
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../unity-asset-yaml/tests/fixtures/SingleDoc.asset");
            env.load_file(&path).unwrap();
            assert!(!env.yaml_documents().is_empty());
            assert!(env.yaml_objects().next().is_some());
            assert!(env.find_yaml_by_anchor("1").is_some());
        }

        #[test]
        fn environment_can_find_binary_object_by_path_id() {
            let mut env = Environment::new();
            let path =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/samples/char_118_yuki.ab");
            env.load_file(&path).unwrap();
            assert!(!env.bundles().is_empty());

            let first = env
                .bundles()
                .values()
                .next()
                .and_then(|b| b.assets.first())
                .and_then(|a| a.objects.first())
                .expect("bundle has at least one object");

            let found = env.find_binary_objects(first.path_id);
            assert!(!found.is_empty());

            // Disambiguation helpers should work on the same source path.
            assert!(
                env.find_binary_object_in_source(&path, first.path_id)
                    .is_some()
            );
            let obj_ref = env
                .find_binary_object_in_bundle_asset(&path, 0, first.path_id)
                .expect("can find object in bundle asset 0");

            let key = obj_ref.key();
            assert_eq!(key.source_path, path);
            assert_eq!(key.source_kind, BinarySourceKind::AssetBundle);
            assert_eq!(key.asset_index, Some(0));
            assert_eq!(key.path_id, first.path_id);

            let key_str = key.to_string();
            let parsed: BinaryObjectKey = key_str.parse().expect("BinaryObjectKey parse");
            assert_eq!(parsed, key);

            let parsed = env.read_binary_object_key(&key).unwrap();
            assert_eq!(parsed.info.path_id, first.path_id);

            let keys = env.find_binary_object_keys(first.path_id);
            assert!(!keys.is_empty());

            let keys_in_source = env.find_binary_object_keys_in_source(&path, first.path_id);
            assert!(keys_in_source.contains(&key));

            // PPtr resolution closure:
            // fileID=0 must resolve to the current serialized file (same source + asset_index).
            let pptr_key = env
                .resolve_binary_pptr(&obj_ref, 0, first.path_id)
                .expect("resolve PPtr with fileID=0");
            assert_eq!(pptr_key, key);

            let pptr_obj = env.read_binary_pptr(&obj_ref, 0, first.path_id).unwrap();
            assert_eq!(pptr_obj.info.path_id, first.path_id);

            // If externals are present, pick an out-of-range fileID; otherwise use 1.
            let invalid_file_id = if obj_ref.object.file().externals.is_empty() {
                1
            } else {
                (obj_ref.object.file().externals.len() as i32) + 1
            };
            assert!(
                env.resolve_binary_pptr(&obj_ref, invalid_file_id, first.path_id)
                    .is_none()
            );
        }

        #[test]
        fn environment_can_read_bundle_container_entries() {
            let mut env = Environment::new();
            let path =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/samples/char_118_yuki.ab");
            env.load_file(&path).unwrap();

            let bundle = env.bundles().get(&path).expect("sample bundle loaded");
            let has_assetbundle_object = bundle
                .assets
                .iter()
                .any(|f| f.objects.iter().any(|o| o.type_id == 142));
            assert!(
                has_assetbundle_object,
                "expected at least one AssetBundle (class id 142) object in sample bundle"
            );

            let entries = env.bundle_container_entries(&path).unwrap();
            assert!(
                !entries.is_empty(),
                "expected at least one m_Container entry in sample bundle"
            );
            assert!(entries.iter().any(|e| !e.asset_path.is_empty()));
            assert!(entries.iter().any(|e| e.key.is_some()));

            let found = env.find_bundle_container_entries(&entries[0].asset_path);
            assert!(!found.is_empty());
        }

        #[test]
        fn environment_can_parse_streamed_audioclip_info_from_raw_bytes() {
            use unity_asset_binary::audio::AudioClipConverter;
            use unity_asset_binary::unity_version::UnityVersion;

            let mut env = Environment::new();
            let path =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/samples/char_118_yuki.ab");
            env.load_file(&path).unwrap();

            let entries = env.bundle_container_entries(&path).unwrap();
            let cn_001 = entries
                .iter()
                .find(|e| e.asset_path.to_ascii_lowercase().ends_with("/cn_001.ogg"))
                .expect("sample bundle contains cn_001.ogg container entry");
            let key = cn_001
                .key
                .clone()
                .expect("cn_001.ogg container entry resolves to an object key");

            let obj = env.read_binary_object_key(&key).unwrap();

            let unity_version = env
                .bundles()
                .get(&path)
                .and_then(|b| key.asset_index.and_then(|i| b.assets.get(i)))
                .and_then(|f| UnityVersion::parse_version(&f.unity_version).ok())
                .unwrap_or_default();

            let converter = AudioClipConverter::new(unity_version);
            let clip = converter.from_unity_object(&obj).unwrap();

            assert!(
                clip.data.is_empty(),
                "streamed clip should not embed audio bytes"
            );
            assert!(clip.is_streamed());
            assert_eq!(clip.stream_info.offset, 4096);
            assert_eq!(clip.stream_info.size, 17088);
            assert!(
                clip.stream_info
                    .path
                    .contains("CAB-8579bc75d50073df38987733a7cb3193")
            );
        }

        #[test]
        fn environment_stream_data_falls_back_to_filesystem_for_bundles() {
            let temp = tempfile::tempdir().unwrap();
            let bundle_src =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/samples/char_118_yuki.ab");
            let bundle_path = temp.path().join("char_118_yuki.ab");
            fs::copy(&bundle_src, &bundle_path).unwrap();

            let cab = "8579bc75d50073df38987733a7cb3193";
            let stream_path = format!("archive:/CAB-{cab}/CAB-{cab}.resource");
            let resource_dir = temp.path().join(format!("CAB-{cab}"));
            fs::create_dir_all(&resource_dir).unwrap();
            let resource_path = resource_dir.join(format!("CAB-{cab}.resource"));

            let mut bytes = vec![0u8; 4096 + 4];
            bytes[4096..4096 + 4].copy_from_slice(b"OggS");
            fs::write(&resource_path, bytes).unwrap();

            let mut env = Environment::new();
            env.load_file(&bundle_path).unwrap();

            let read = env
                .read_stream_data(
                    &bundle_path,
                    BinarySourceKind::AssetBundle,
                    &stream_path,
                    4096,
                    4,
                )
                .unwrap();
            assert_eq!(read, b"OggS");
        }

        #[test]
        fn environment_stream_data_fs_supports_cab_suffix_files() {
            let temp = tempfile::tempdir().unwrap();
            let bundle_src =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/samples/char_118_yuki.ab");
            let bundle_path = temp.path().join("char_118_yuki.ab");
            fs::copy(&bundle_src, &bundle_path).unwrap();

            let cab = "8579bc75d50073df38987733a7cb3193";
            let stream_path = format!("archive:/CAB-{cab}/CAB-{cab}.resource");

            // Common on-disk variant: `CAB-<hash>1.resource` (no folder).
            let resource_path = temp.path().join(format!("CAB-{cab}1.resource"));
            let mut bytes = vec![0u8; 4096 + 4];
            bytes[4096..4096 + 4].copy_from_slice(b"OggS");
            fs::write(&resource_path, bytes).unwrap();

            let mut env = Environment::new();
            env.load_file(&bundle_path).unwrap();

            let read = env
                .read_stream_data(
                    &bundle_path,
                    BinarySourceKind::AssetBundle,
                    &stream_path,
                    4096,
                    4,
                )
                .unwrap();
            assert_eq!(read, b"OggS");
        }
    }
}
