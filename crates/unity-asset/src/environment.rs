//! Environment for managing multiple Unity assets.
//!
//! This module hosts the high-level `Environment` API, which provides:
//! - multi-source loading (bundles, serialized files, web files)
//! - container discovery (`m_Container`) and object key resolution
//! - streamed resource reads (`.resS` / `.resource`) with best-effort fallbacks
//! - strict/lenient TypeTree parsing knobs + structured warnings

mod imp {
    use crate::{Result, YamlDocument};
    use std::collections::HashMap;
    use std::fmt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, RwLock};
    use unity_asset_binary::asset::SerializedFile;
    use unity_asset_binary::bundle::AssetBundle;
    use unity_asset_binary::file::{UnityFile, load_unity_file, load_unity_file_from_shared_range};
    use unity_asset_binary::object::{ObjectHandle, UnityObject};
    use unity_asset_binary::typetree::TypeTreeRegistry;
    use unity_asset_binary::typetree::{
        CompositeTypeTreeRegistry, JsonTypeTreeRegistry, TpkTypeTreeRegistry,
    };
    use unity_asset_binary::typetree::{
        TypeTreeParseMode, TypeTreeParseOptions, TypeTreeParseWarning,
    };
    use unity_asset_binary::webfile::WebFile;
    use unity_asset_core::UnityValue;
    use unity_asset_core::{UnityAssetError, UnityClass, UnityDocument};

    mod container;
    mod dependency_graph;
    mod edit;
    mod key;
    mod loader;
    mod meta_guid;
    mod object_graph;
    mod object_query;
    mod path;
    mod pptr;
    mod save;
    mod stream;
    mod streamed_write;

    pub use dependency_graph::{
        DependencyGraphBuildOptions, DependencyGraphTraversalOptions, DependencyGraphWarning,
        EnvironmentDependencyGraph, ExternalDependencyEdge,
    };
    pub use edit::{EnvironmentEditSession, StreamedResourceWrite};
    pub use loader::{ProjectLoadOptions, ProjectLoadStats};
    pub use object_graph::{
        EnvironmentObjectGraph, EnvironmentObjectKey, ExternalObjectEdge, ObjectGraphBuildOptions,
        ObjectGraphTraversalOptions, YamlExternalEdge, YamlObjectKey,
    };

    #[derive(Debug, Clone)]
    pub enum EnvironmentWarning {
        LoadFailed {
            path: PathBuf,
            error: String,
        },
        YamlDocumentSkipped {
            path: PathBuf,
            doc_index: usize,
            error: String,
        },
    }

    impl fmt::Display for EnvironmentWarning {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                EnvironmentWarning::LoadFailed { path, error } => {
                    write!(f, "Failed to load {}: {}", path.to_string_lossy(), error)
                }
                EnvironmentWarning::YamlDocumentSkipped {
                    path,
                    doc_index,
                    error,
                } => write!(
                    f,
                    "YAML warning in {} (doc {}): {}",
                    path.to_string_lossy(),
                    doc_index,
                    error
                ),
            }
        }
    }

    pub trait EnvironmentReporter: Send + Sync {
        fn warn(&self, warning: &EnvironmentWarning);
        fn typetree_warning(&self, _key: &BinaryObjectKey, _warning: &TypeTreeParseWarning) {}
    }

    #[derive(Debug, Default)]
    pub struct NoopReporter;

    impl EnvironmentReporter for NoopReporter {
        fn warn(&self, _warning: &EnvironmentWarning) {}
    }

    #[derive(Debug, Clone, Copy)]
    pub struct EnvironmentOptions {
        pub typetree: TypeTreeParseOptions,
    }

    impl EnvironmentOptions {
        pub fn strict() -> Self {
            Self {
                typetree: TypeTreeParseOptions {
                    mode: TypeTreeParseMode::Strict,
                },
            }
        }

        pub fn lenient() -> Self {
            Self {
                typetree: TypeTreeParseOptions {
                    mode: TypeTreeParseMode::Lenient,
                },
            }
        }
    }

    impl Default for EnvironmentOptions {
        fn default() -> Self {
            Self::lenient()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub enum BinarySource {
        Path(PathBuf),
        WebEntry {
            web_path: PathBuf,
            entry_name: String,
        },
    }

    impl fmt::Display for BinarySource {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                BinarySource::Path(p) => write!(f, "{}", p.to_string_lossy()),
                BinarySource::WebEntry {
                    web_path,
                    entry_name,
                } => write!(f, "{}::{}", web_path.to_string_lossy(), entry_name),
            }
        }
    }

    impl BinarySource {
        pub fn path<P: AsRef<Path>>(path: P) -> Self {
            Self::Path(path.as_ref().to_path_buf())
        }

        pub fn describe(&self) -> String {
            self.to_string()
        }

        fn as_path(&self) -> Option<&PathBuf> {
            match self {
                BinarySource::Path(p) => Some(p),
                BinarySource::WebEntry { .. } => None,
            }
        }
    }

    /// A reference to a binary object within a `SerializedFile`.
    ///
    /// This is conceptually similar to UnityPy's `ObjectReader`: it is a lightweight handle that can be
    /// converted into a parsed `UnityObject` on-demand.
    #[derive(Clone)]
    pub struct BinaryObjectRef<'a> {
        pub source: &'a BinarySource,
        pub source_kind: BinarySourceKind,
        /// Asset index within a bundle. `None` for standalone serialized files.
        pub asset_index: Option<usize>,
        pub object: ObjectHandle<'a>,
        typetree_options: TypeTreeParseOptions,
        reporter: Option<Arc<dyn EnvironmentReporter>>,
    }

    impl<'a> fmt::Debug for BinaryObjectRef<'a> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("BinaryObjectRef")
                .field("source", &self.source)
                .field("source_kind", &self.source_kind)
                .field("asset_index", &self.asset_index)
                .field("path_id", &self.object.path_id())
                .finish()
        }
    }

    impl<'a> BinaryObjectRef<'a> {
        pub fn read(&self) -> Result<UnityObject> {
            let obj = self
                .object
                .read_with_options(self.typetree_options)
                .map_err(|e| {
                    UnityAssetError::format(format!("Failed to parse binary object: {}", e))
                })?;

            if let Some(reporter) = &self.reporter {
                let key = self.key();
                for w in obj.typetree_warnings() {
                    reporter.typetree_warning(&key, w);
                }
            }

            Ok(obj)
        }

        /// Create a globally-unique key for this object reference.
        pub fn key(&self) -> BinaryObjectKey {
            BinaryObjectKey {
                source: self.source.clone(),
                source_kind: self.source_kind,
                asset_index: self.asset_index,
                path_id: self.object.path_id(),
            }
        }
    }

    /// A unified object reference across YAML and binary formats.
    #[derive(Debug, Clone)]
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
        pub source: BinarySource,
        pub source_kind: BinarySourceKind,
        pub asset_index: Option<usize>,
        pub path_id: i64,
    }

    /// A best-effort entry extracted from an AssetBundle `m_Container`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BundleContainerEntry {
        pub bundle_source: BinarySource,
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
        binary_assets: HashMap<BinarySource, SerializedFile>,
        /// Loaded AssetBundles (e.g. `.bundle`, `.unity3d`, `.ab`)
        bundles: HashMap<BinarySource, AssetBundle>,
        webfiles: HashMap<PathBuf, WebFile>,
        bundle_container_cache: RwLock<HashMap<BinarySource, Vec<BundleContainerEntry>>>,
        dependency_scan_cache: RwLock<dependency_graph::DependencyScanCache>,
        meta_guid_cache: RwLock<HashMap<[u8; 16], PathBuf>>,
        warnings: Mutex<Vec<EnvironmentWarning>>,
        reporter: Option<Arc<dyn EnvironmentReporter>>,
        options: EnvironmentOptions,
        type_tree_registry: Option<Arc<dyn TypeTreeRegistry>>,
        write_state: edit::EnvironmentWriteState,
        /// Base path for relative file resolution
        #[allow(dead_code)]
        base_path: PathBuf,
    }

    impl Environment {
        /// Create a new environment
        pub fn new() -> Self {
            Self::with_options(EnvironmentOptions::default())
        }

        pub fn with_options(options: EnvironmentOptions) -> Self {
            Self {
                yaml_documents: HashMap::new(),
                binary_assets: HashMap::new(),
                bundles: HashMap::new(),
                webfiles: HashMap::new(),
                bundle_container_cache: RwLock::new(HashMap::new()),
                dependency_scan_cache: RwLock::new(HashMap::new()),
                meta_guid_cache: RwLock::new(HashMap::new()),
                warnings: Mutex::new(Vec::new()),
                reporter: None,
                options,
                type_tree_registry: None,
                write_state: edit::EnvironmentWriteState::default(),
                base_path: std::env::current_dir().unwrap_or_default(),
            }
        }

        pub fn set_reporter(&mut self, reporter: Option<Arc<dyn EnvironmentReporter>>) {
            self.reporter = reporter;
        }

        pub fn set_type_tree_registry(&mut self, registry: Option<Arc<dyn TypeTreeRegistry>>) {
            self.type_tree_registry = registry.clone();

            for file in self.binary_assets.values_mut() {
                file.set_type_tree_registry(registry.clone());
            }
            for bundle in self.bundles.values_mut() {
                for file in bundle.assets.iter_mut() {
                    file.set_type_tree_registry(registry.clone());
                }
            }
        }

        /// Load and set an external TypeTree registry from a list of file paths.
        ///
        /// Supported formats:
        /// - `.tpk` (UnityPy/UABEA Type Package registry)
        /// - `.json` (this project's JSON registry format)
        ///
        /// When multiple paths are provided, they are composed in the given order (first match wins).
        pub fn set_type_tree_registry_from_paths(&mut self, paths: &[PathBuf]) -> Result<()> {
            if paths.is_empty() {
                self.set_type_tree_registry(None);
                return Ok(());
            }

            let mut composite = CompositeTypeTreeRegistry::default();
            for path in paths {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();

                if ext == "tpk" {
                    let registry = TpkTypeTreeRegistry::from_path(path).map_err(|e| {
                        UnityAssetError::format(format!(
                            "Failed to load TypeTree registry {:?}: {}",
                            path, e
                        ))
                    })?;
                    composite.push(Arc::new(registry));
                } else {
                    let registry = JsonTypeTreeRegistry::from_path(path).map_err(|e| {
                        UnityAssetError::format(format!(
                            "Failed to load TypeTree registry {:?}: {}",
                            path, e
                        ))
                    })?;
                    composite.push(Arc::new(registry));
                }
            }

            if composite.is_empty() {
                self.set_type_tree_registry(None);
                return Ok(());
            }

            self.set_type_tree_registry(Some(Arc::new(composite)));
            Ok(())
        }

        pub fn options(&self) -> EnvironmentOptions {
            self.options
        }

        pub fn warnings(&self) -> Vec<EnvironmentWarning> {
            match self.warnings.lock() {
                Ok(v) => v.clone(),
                Err(e) => e.into_inner().clone(),
            }
        }

        pub fn take_warnings(&self) -> Vec<EnvironmentWarning> {
            match self.warnings.lock() {
                Ok(mut v) => std::mem::take(&mut *v),
                Err(e) => {
                    let mut v = e.into_inner();
                    std::mem::take(&mut *v)
                }
            }
        }

        fn push_warning(&self, warning: EnvironmentWarning) {
            match self.warnings.lock() {
                Ok(mut warnings) => warnings.push(warning.clone()),
                Err(e) => e.into_inner().push(warning.clone()),
            }
            if let Some(reporter) = &self.reporter {
                reporter.warn(&warning);
            }
        }

        /// Iterate YAML Unity objects.
        pub fn yaml_objects(&self) -> impl Iterator<Item = &UnityClass> {
            self.yaml_documents.values().flat_map(|doc| doc.entries())
        }

        /// Find a YAML object by its YAML anchor (the `&<id>` part).
        pub fn find_yaml_by_anchor(&self, anchor: &str) -> Option<&UnityClass> {
            self.yaml_objects().find(|obj| obj.anchor == anchor)
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
        pub fn binary_assets(&self) -> &HashMap<BinarySource, SerializedFile> {
            &self.binary_assets
        }

        /// Get loaded AssetBundles.
        pub fn bundles(&self) -> &HashMap<BinarySource, AssetBundle> {
            &self.bundles
        }

        /// Get loaded WebFiles (containers).
        pub fn webfiles(&self) -> &HashMap<PathBuf, WebFile> {
            &self.webfiles
        }
    }

    impl Default for Environment {
        fn default() -> Self {
            Self::new()
        }
    }

    #[cfg(test)]
    mod tests;
}

pub use imp::*;
