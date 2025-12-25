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
    use std::str::FromStr;
    use std::sync::{Arc, Mutex, RwLock};
    use unity_asset_binary::asset::SerializedFile;
    use unity_asset_binary::bundle::AssetBundle;
    use unity_asset_binary::file::{UnityFile, load_unity_file, load_unity_file_from_shared_range};
    use unity_asset_binary::object::{ObjectHandle, UnityObject};
    use unity_asset_binary::typetree::TypeTreeRegistry;
    use unity_asset_binary::typetree::{
        TypeTreeParseMode, TypeTreeParseOptions, TypeTreeParseWarning,
    };
    use unity_asset_binary::webfile::WebFile;
    use unity_asset_core::UnityValue;
    use unity_asset_core::{UnityAssetError, UnityClass, UnityDocument};

    mod container;
    mod loader;
    mod object_query;
    mod pptr;
    mod stream;

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

    impl std::fmt::Display for BinaryObjectKey {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // A copy/paste friendly key format that can be parsed back with `FromStr`.
            //
            // bok1 (legacy):
            //   bok1|<kind>|<asset_index_or_dash>|<path_id>|<path_utf8_len>|<path>
            //
            // bok2:
            //   bok2|<kind>|<asset_index_or_dash>|<path_id>|<outer_utf8_len>|<outer>|<entry_utf8_len>|<entry>
            //
            // Notes:
            // - `<outer>` is either the filesystem path or the WebFile path.
            // - `<entry>` is empty for filesystem paths; otherwise it's the WebFile entry name.
            // - `<outer>` and `<entry>` can contain `|` because their UTF-8 lengths are encoded.
            let kind = match self.source_kind {
                BinarySourceKind::SerializedFile => "serialized",
                BinarySourceKind::AssetBundle => "bundle",
            };
            let asset_index = self
                .asset_index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".to_string());

            let (outer, entry) = match &self.source {
                BinarySource::Path(p) => (p.to_string_lossy().to_string(), String::new()),
                BinarySource::WebEntry {
                    web_path,
                    entry_name,
                } => (web_path.to_string_lossy().to_string(), entry_name.clone()),
            };
            write!(
                f,
                "bok2|{}|{}|{}|{}|{}|{}|{}",
                kind,
                asset_index,
                self.path_id,
                outer.as_bytes().len(),
                outer,
                entry.as_bytes().len(),
                entry
            )
        }
    }

    impl FromStr for BinaryObjectKey {
        type Err = String;

        fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
            if s.starts_with("bok2|") {
                return parse_bok2(s);
            }
            if s.starts_with("bok1|") {
                return parse_bok1(s);
            }
            Err("invalid key prefix (expected: bok1|... or bok2|...)".to_string())
        }
    }

    fn parse_kind(kind: &str) -> std::result::Result<BinarySourceKind, String> {
        match kind {
            "bundle" => Ok(BinarySourceKind::AssetBundle),
            "serialized" => Ok(BinarySourceKind::SerializedFile),
            other => Err(format!("unknown kind: {}", other)),
        }
    }

    fn parse_asset_index(asset_index: &str) -> std::result::Result<Option<usize>, String> {
        if asset_index == "-" || asset_index.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            asset_index
                .parse::<usize>()
                .map_err(|e| format!("invalid asset_index: {}", e))?,
        ))
    }

    fn parse_bok1(s: &str) -> std::result::Result<BinaryObjectKey, String> {
        let prefix = "bok1|";
        let mut rest = &s[prefix.len()..];
        let (kind, r) = split_once(rest, '|').ok_or_else(|| "missing kind".to_string())?;
        rest = r;
        let (asset_index, r) =
            split_once(rest, '|').ok_or_else(|| "missing asset_index".to_string())?;
        rest = r;
        let (path_id, r) = split_once(rest, '|').ok_or_else(|| "missing path_id".to_string())?;
        rest = r;
        let (path_len, path) =
            split_once(rest, '|').ok_or_else(|| "missing path_len/path".to_string())?;

        let source_kind = parse_kind(kind)?;
        let asset_index = parse_asset_index(asset_index)?;
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

        Ok(BinaryObjectKey {
            source: BinarySource::Path(PathBuf::from(path)),
            source_kind,
            asset_index,
            path_id,
        })
    }

    fn parse_bok2(s: &str) -> std::result::Result<BinaryObjectKey, String> {
        let prefix = "bok2|";
        let mut rest = &s[prefix.len()..];

        let (kind, r) = split_once(rest, '|').ok_or_else(|| "missing kind".to_string())?;
        rest = r;
        let (asset_index, r) =
            split_once(rest, '|').ok_or_else(|| "missing asset_index".to_string())?;
        rest = r;
        let (path_id, r) = split_once(rest, '|').ok_or_else(|| "missing path_id".to_string())?;
        rest = r;
        let (outer_len, r) =
            split_once(rest, '|').ok_or_else(|| "missing outer_len".to_string())?;
        rest = r;

        let source_kind = parse_kind(kind)?;
        let asset_index = parse_asset_index(asset_index)?;
        let path_id = path_id
            .parse::<i64>()
            .map_err(|e| format!("invalid path_id: {}", e))?;

        let outer_len = outer_len
            .parse::<usize>()
            .map_err(|e| format!("invalid outer_len: {}", e))?;
        if rest.as_bytes().len() < outer_len {
            return Err("outer is shorter than outer_len".to_string());
        }

        let outer = rest
            .get(..outer_len)
            .ok_or_else(|| "outer_len splits UTF-8 boundary".to_string())?;
        let rest = rest
            .get(outer_len..)
            .ok_or_else(|| "outer_len splits UTF-8 boundary".to_string())?;

        let rest = rest
            .strip_prefix('|')
            .ok_or_else(|| "missing entry delimiter".to_string())?;
        let (entry_len, rest) =
            split_once(rest, '|').ok_or_else(|| "missing entry_len".to_string())?;
        let entry_len = entry_len
            .parse::<usize>()
            .map_err(|e| format!("invalid entry_len: {}", e))?;
        if rest.as_bytes().len() != entry_len {
            return Err(format!(
                "entry length mismatch: expected {} bytes, got {} bytes",
                entry_len,
                rest.as_bytes().len()
            ));
        }

        if source_kind == BinarySourceKind::AssetBundle && asset_index.is_none() {
            return Err("asset_index is required for bundle keys".to_string());
        }

        let source = if entry_len == 0 {
            BinarySource::Path(PathBuf::from(outer))
        } else {
            BinarySource::WebEntry {
                web_path: PathBuf::from(outer),
                entry_name: rest.to_string(),
            }
        };

        Ok(BinaryObjectKey {
            source,
            source_kind,
            asset_index,
            path_id,
        })
    }

    fn split_once<'a>(s: &'a str, delim: char) -> Option<(&'a str, &'a str)> {
        let pos = s.find(delim)?;
        Some((&s[..pos], &s[pos + delim.len_utf8()..]))
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
        warnings: Mutex<Vec<EnvironmentWarning>>,
        reporter: Option<Arc<dyn EnvironmentReporter>>,
        options: EnvironmentOptions,
        type_tree_registry: Option<Arc<dyn TypeTreeRegistry>>,
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
                warnings: Mutex::new(Vec::new()),
                reporter: None,
                options,
                type_tree_registry: None,
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
        fn environment_can_find_binary_object_by_path_id_and_container_and_stream_info() {
            use unity_asset_binary::unity_version::UnityVersion;
            use unity_asset_decode::audio::AudioClipConverter;

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
            assert_eq!(key.source, BinarySource::path(&path));
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

            let bundle = env
                .bundles()
                .get(&BinarySource::path(&path))
                .expect("sample bundle loaded");
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
                .get(&BinarySource::path(&path))
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

            let peek = env.peek_binary_object_name(&key).unwrap();
            assert_eq!(peek, obj.name());
        }

        #[test]
        fn environment_typetree_registry_json_restores_parsing_for_stripped_assets() {
            use serde::Serialize;
            use std::sync::Arc;
            use unity_asset_binary::typetree::JsonTypeTreeRegistry;

            #[derive(Debug, Serialize)]
            struct Dump {
                schema: u32,
                entries: Vec<Entry>,
            }

            #[derive(Debug, Serialize)]
            struct Entry {
                #[serde(skip_serializing_if = "Option::is_none")]
                unity_version: Option<String>,
                class_id: i32,
                type_tree: unity_asset_binary::typetree::TypeTree,
            }

            let mut env = Environment::new();
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/samples/banner_1");
            env.load_file(&path).unwrap();

            let source = BinarySource::path(&path);
            let texture_path_id = -3875358842991402074i64;
            let key = BinaryObjectKey {
                source: source.clone(),
                source_kind: BinarySourceKind::AssetBundle,
                asset_index: Some(0),
                path_id: texture_path_id,
            };

            let type_tree = {
                let bundle = env.bundles.get(&source).expect("sample bundle loaded");
                let file = bundle.assets.get(0).expect("bundle has asset 0");
                file.types
                    .iter()
                    .find(|t| t.class_id == 28)
                    .expect("bundle asset has Texture2D type tree")
                    .type_tree
                    .clone()
            };

            {
                let bundle = env
                    .bundles
                    .get_mut(&source)
                    .expect("sample bundle loaded (mutable)");
                let file = bundle.assets.get_mut(0).expect("bundle has asset 0");
                file.enable_type_tree = false;
                for t in file.types.iter_mut() {
                    t.type_tree.clear();
                }
                file.set_type_tree_registry(None);
            }

            let obj = env.read_binary_object_key(&key).unwrap();
            assert_eq!(obj.name(), None, "expected no typetree without registry");

            let tmp = tempfile::tempdir().unwrap();
            let reg_path = tmp.path().join("typetree_registry.json");
            let dump = Dump {
                schema: 1,
                entries: vec![Entry {
                    unity_version: None,
                    class_id: 28,
                    type_tree,
                }],
            };
            fs::write(&reg_path, serde_json::to_string_pretty(&dump).unwrap()).unwrap();

            let registry = JsonTypeTreeRegistry::from_path(&reg_path).unwrap();
            env.set_type_tree_registry(Some(Arc::new(registry)));

            let obj = env.read_binary_object_key(&key).unwrap();
            assert_eq!(obj.name().as_deref(), Some("banner_1"));
            assert_eq!(obj.get("m_Width").and_then(|v| v.as_i64()), Some(492));
            assert_eq!(obj.get("m_Height").and_then(|v| v.as_i64()), Some(180));
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

            // Common on-disk variant: `CAB-<hash>1.resource` (no folder).
            fs::remove_file(&resource_path).unwrap();
            fs::remove_dir_all(&resource_dir).unwrap();

            let resource_path = temp.path().join(format!("CAB-{cab}1.resource"));
            let mut bytes = vec![0u8; 4096 + 4];
            bytes[4096..4096 + 4].copy_from_slice(b"OggS");
            fs::write(&resource_path, bytes).unwrap();

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

        fn build_uncompressed_webfile(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
            let signature = b"UnityWebData1.0\0";

            let entry_table_len: usize = entries
                .iter()
                .map(|(name, _)| 12usize.saturating_add(name.as_bytes().len()))
                .sum();
            let header_len: usize = signature
                .len()
                .saturating_add(std::mem::size_of::<i32>())
                .saturating_add(entry_table_len);

            let head_length_i32: i32 = header_len
                .try_into()
                .expect("header_len fits i32 for test webfile");

            let mut out: Vec<u8> = Vec::with_capacity(
                header_len.saturating_add(entries.iter().map(|(_, b)| b.len()).sum::<usize>()),
            );
            out.extend_from_slice(signature);
            out.extend_from_slice(&head_length_i32.to_le_bytes());

            let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
            let mut cursor = header_len;

            for (name, bytes) in entries {
                let offset_i32: i32 = cursor.try_into().expect("offset fits i32");
                let length_i32: i32 = bytes.len().try_into().expect("length fits i32");
                let name_len_i32: i32 = name.len().try_into().expect("name_len fits i32");

                out.extend_from_slice(&offset_i32.to_le_bytes());
                out.extend_from_slice(&length_i32.to_le_bytes());
                out.extend_from_slice(&name_len_i32.to_le_bytes());
                out.extend_from_slice(name.as_bytes());

                cursor = cursor.saturating_add(bytes.len());
                payloads.push(bytes);
            }

            for payload in payloads {
                out.extend_from_slice(&payload);
            }

            out
        }

        #[test]
        fn environment_loads_extless_webfile_entries_and_reads_resource_bytes() {
            let sample_bundle_path =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/samples/char_118_yuki.ab");
            let bundle_bytes = fs::read(&sample_bundle_path).unwrap();

            let cab = "8579bc75d50073df38987733a7cb3193";
            let resource_name = format!("CAB-{cab}.resource");
            let mut resource_bytes = vec![0u8; 4096 + 4];
            resource_bytes[4096..4096 + 4].copy_from_slice(b"OggS");

            let entry_name = "char_118_yuki.ab".to_string();
            let web_bytes = build_uncompressed_webfile(vec![
                (entry_name.clone(), bundle_bytes),
                (resource_name.clone(), resource_bytes),
            ]);

            let temp = tempfile::tempdir().unwrap();
            let web_path = temp.path().join("UnityWebData");
            fs::write(&web_path, web_bytes).unwrap();

            let mut env = Environment::new();
            env.load_file(&web_path).unwrap();
            assert!(env.webfiles().contains_key(&web_path));

            let bundle_source = BinarySource::WebEntry {
                web_path: web_path.clone(),
                entry_name,
            };
            assert!(env.bundles().contains_key(&bundle_source));

            let obj_ref = env
                .binary_object_infos()
                .find(|r| {
                    r.source == &bundle_source && r.source_kind == BinarySourceKind::AssetBundle
                })
                .expect("web bundle yields at least one object handle");

            let key = obj_ref.key();
            assert_eq!(key.source, bundle_source);

            let key_str = key.to_string();
            let parsed: BinaryObjectKey = key_str.parse().expect("BinaryObjectKey parse");
            assert_eq!(parsed, key);

            let stream_path = format!("archive:/CAB-{cab}/{resource_name}");
            let read = env
                .read_stream_data_source(
                    &key.source,
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

pub use imp::*;
