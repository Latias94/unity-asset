use crate::typetree::{TypeTree, TypeTreeNode, TypeTreeRegistry, TypeTreeSerializationMode};
use serde::{Deserialize, Deserializer};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AssetRipperTypeTreeGeneratorRegistryError {
    #[error("failed to deserialize AssetRipper TypeTree JSON: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("invalid AssetRipper TypeTree format: {0}")]
    FileFormatError(String),

    #[error("AssetRipper TypeTree I/O error: {0}")]
    IOError(#[from] io::Error),

    #[error("failed to access AssetRipper TypeTree path {path}: {source}")]
    PathIOError {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to deserialize AssetRipper TypeTree JSON at {path}: {source}")]
    PathSerdeError {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AssetRipperDump {
    version: String,
    classes: Vec<AssetRipperClass>,
}

#[derive(Debug, Deserialize)]
struct AssetRipperClass {
    #[serde(rename = "TypeID")]
    type_id: i32,
    #[serde(
        rename = "EditorRootNode",
        deserialize_with = "deserialize_required_option"
    )]
    editor_root_node: Option<AssetRipperNode>,
    #[serde(
        rename = "ReleaseRootNode",
        deserialize_with = "deserialize_required_option"
    )]
    release_root_node: Option<AssetRipperNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AssetRipperNode {
    type_name: String,
    name: String,
    level: u8,
    byte_size: i32,
    index: i32,
    version: u16,
    type_flags: u8,
    meta_flag: u32,
    sub_nodes: Vec<AssetRipperNode>,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl From<AssetRipperNode> for TypeTreeNode {
    fn from(node: AssetRipperNode) -> Self {
        Self {
            type_name: node.type_name,
            name: node.name,
            byte_size: node.byte_size,
            variable_count: 0,
            index: node.index,
            type_flags: i32::from(node.type_flags),
            version: i32::from(node.version),
            meta_flags: i32::from_ne_bytes(node.meta_flag.to_ne_bytes()),
            level: i32::from(node.level),
            type_str_offset: 0,
            name_str_offset: 0,
            ref_type_hash: 0,
            children: node.sub_nodes.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone)]
struct ClassTypeTrees {
    release: Option<Arc<TypeTree>>,
    editor: Option<Arc<TypeTree>>,
}

impl ClassTypeTrees {
    fn resolve(&self, mode: TypeTreeSerializationMode) -> Option<Arc<TypeTree>> {
        match mode {
            TypeTreeSerializationMode::Release => self.release.clone(),
            TypeTreeSerializationMode::Editor => self.editor.clone(),
        }
    }
}

type ClassesById = HashMap<i32, ClassTypeTrees>;

#[derive(Debug, Default)]
struct AssetRipperRegistryState {
    exact: HashMap<String, ClassesById>,
    loaded_sources: HashSet<String>,
}

impl AssetRipperRegistryState {
    fn insert_dump(&mut self, dump: AssetRipperDump) {
        let classes = dump
            .classes
            .into_iter()
            .filter_map(convert_class)
            .collect::<ClassesById>();

        self.exact.entry(dump.version).or_default().extend(classes);
    }

    fn resolve(
        &self,
        unity_version: &str,
        class_id: i32,
        mode: TypeTreeSerializationMode,
    ) -> Option<Arc<TypeTree>> {
        self.exact
            .get(unity_version)
            .and_then(|classes| classes.get(&class_id))
            .and_then(|trees| trees.resolve(mode))
    }
}

#[derive(Debug)]
pub struct AssetRipperTypeTreeGeneratorRegistry {
    inner: RwLock<AssetRipperRegistryState>,
    version_sources: HashMap<String, PathBuf>,
}

impl AssetRipperTypeTreeGeneratorRegistry {
    fn new() -> Self {
        Self {
            inner: RwLock::new(AssetRipperRegistryState::default()),
            version_sources: HashMap::new(),
        }
    }

    pub fn new_from_path(
        path: impl AsRef<Path>,
    ) -> Result<Self, AssetRipperTypeTreeGeneratorRegistryError> {
        let mut registry = Self::new();
        registry.add_via_path(path)?;
        Ok(registry)
    }

    pub fn new_from_reader(
        reader: impl Read,
    ) -> Result<Self, AssetRipperTypeTreeGeneratorRegistryError> {
        let mut registry = Self::new();
        registry.add_via_reader(reader)?;
        Ok(registry)
    }

    pub fn add_via_path(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<(), AssetRipperTypeTreeGeneratorRegistryError> {
        let path = path.as_ref();
        if path.is_dir() {
            self.index_directory(path)
        } else {
            let dump = read_dump_from_path(path)?;
            self.state_mut().insert_dump(dump);
            Ok(())
        }
    }

    pub fn add_via_reader(
        &mut self,
        reader: impl Read,
    ) -> Result<(), AssetRipperTypeTreeGeneratorRegistryError> {
        let dump = serde_json::from_reader(reader)?;
        validate_dump(&dump)?;
        self.state_mut().insert_dump(dump);
        Ok(())
    }

    /// Load and validate the indexed JSON file for one exact Unity version.
    ///
    /// Returns `Ok(false)` when the registry has no file indexed for the version.
    pub fn load_version(
        &self,
        version: &str,
    ) -> Result<bool, AssetRipperTypeTreeGeneratorRegistryError> {
        let state = self.state();
        if state.exact.contains_key(version) || state.loaded_sources.contains(version) {
            return Ok(true);
        }
        drop(state);

        let Some(path) = self.version_sources.get(version) else {
            return Ok(false);
        };
        let dump = read_dump_from_path(path)?;
        if dump.version != version {
            return Err(AssetRipperTypeTreeGeneratorRegistryError::FileFormatError(
                format!(
                    "indexed file {} declares Version {:?}, expected {:?}",
                    path.display(),
                    dump.version,
                    version
                ),
            ));
        }

        let mut state = self.state_mut();
        if state.loaded_sources.insert(version.to_owned()) {
            state.insert_dump(dump);
        }
        Ok(true)
    }

    fn index_directory(
        &mut self,
        directory: &Path,
    ) -> Result<(), AssetRipperTypeTreeGeneratorRegistryError> {
        let info_json = directory.join("InfoJson");
        let directory = if info_json.is_dir() {
            info_json
        } else {
            directory.to_owned()
        };

        let entries = fs::read_dir(&directory).map_err(|source| {
            AssetRipperTypeTreeGeneratorRegistryError::PathIOError {
                path: directory.clone(),
                source,
            }
        })?;
        let mut indexed_files = 0usize;
        for entry in entries {
            let entry =
                entry.map_err(
                    |source| AssetRipperTypeTreeGeneratorRegistryError::PathIOError {
                        path: directory.clone(),
                        source,
                    },
                )?;
            let path = entry.path();
            if !path.is_file()
                || !path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            {
                continue;
            }
            let Some(version) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            self.version_sources.insert(version.to_owned(), path);
            indexed_files += 1;
        }
        if indexed_files == 0 {
            return Err(AssetRipperTypeTreeGeneratorRegistryError::FileFormatError(
                format!(
                    "AssetRipper TypeTree directory {} contains no .json files",
                    directory.display()
                ),
            ));
        }
        Ok(())
    }

    fn state(&self) -> std::sync::RwLockReadGuard<'_, AssetRipperRegistryState> {
        self.inner.read().unwrap_or_else(|error| error.into_inner())
    }

    fn state_mut(&self) -> std::sync::RwLockWriteGuard<'_, AssetRipperRegistryState> {
        self.inner
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }
}

impl TypeTreeRegistry for AssetRipperTypeTreeGeneratorRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        self.resolve_with_mode(unity_version, class_id, TypeTreeSerializationMode::Release)
    }

    fn resolve_with_mode(
        &self,
        unity_version: &str,
        class_id: i32,
        mode: TypeTreeSerializationMode,
    ) -> Option<Arc<TypeTree>> {
        self.load_version(unity_version).ok()?;
        self.state().resolve(unity_version, class_id, mode)
    }
}

fn convert_class(class: AssetRipperClass) -> Option<(i32, ClassTypeTrees)> {
    let release = class.release_root_node.map(type_tree_from_root);
    let editor = class.editor_root_node.map(type_tree_from_root);
    if release.is_none() && editor.is_none() {
        return None;
    }
    Some((class.type_id, ClassTypeTrees { release, editor }))
}

fn type_tree_from_root(root: AssetRipperNode) -> Arc<TypeTree> {
    let mut tree = TypeTree::new();
    tree.nodes.push(root.into());
    Arc::new(tree)
}

fn validate_dump(dump: &AssetRipperDump) -> Result<(), AssetRipperTypeTreeGeneratorRegistryError> {
    if dump.version.is_empty() || dump.version.contains('*') {
        return Err(AssetRipperTypeTreeGeneratorRegistryError::FileFormatError(
            format!(
                "Version {:?} must be a non-empty exact Unity version without wildcards",
                dump.version
            ),
        ));
    }

    for class in &dump.classes {
        if let Some(root) = &class.editor_root_node {
            validate_node(&dump.version, class.type_id, "EditorRootNode", root)?;
        }
        if let Some(root) = &class.release_root_node {
            validate_node(&dump.version, class.type_id, "ReleaseRootNode", root)?;
        }
    }
    Ok(())
}

fn validate_node(
    version: &str,
    class_id: i32,
    path: &str,
    node: &AssetRipperNode,
) -> Result<(), AssetRipperTypeTreeGeneratorRegistryError> {
    if node.type_name.is_empty() {
        return Err(invalid_node_field(
            version,
            class_id,
            path,
            "TypeName",
            "must not be empty",
        ));
    }
    if node.name.is_empty() {
        return Err(invalid_node_field(
            version,
            class_id,
            path,
            "Name",
            "must not be empty",
        ));
    }
    if node.byte_size < -1 {
        return Err(invalid_node_field(
            version,
            class_id,
            path,
            "ByteSize",
            &format!("must be at least -1, got {}", node.byte_size),
        ));
    }

    for (index, child) in node.sub_nodes.iter().enumerate() {
        validate_node(
            version,
            class_id,
            &format!("{path}.SubNodes[{index}]"),
            child,
        )?;
    }
    Ok(())
}

fn invalid_node_field(
    version: &str,
    class_id: i32,
    path: &str,
    field: &str,
    detail: &str,
) -> AssetRipperTypeTreeGeneratorRegistryError {
    AssetRipperTypeTreeGeneratorRegistryError::FileFormatError(format!(
        "Version {version:?}, class {class_id}, {path}.{field} {detail}"
    ))
}

fn read_dump_from_path(
    path: &Path,
) -> Result<AssetRipperDump, AssetRipperTypeTreeGeneratorRegistryError> {
    let file = File::open(path).map_err(|source| {
        AssetRipperTypeTreeGeneratorRegistryError::PathIOError {
            path: path.to_owned(),
            source,
        }
    })?;
    let dump = serde_json::from_reader(BufReader::new(file)).map_err(|source| {
        AssetRipperTypeTreeGeneratorRegistryError::PathSerdeError {
            path: path.to_owned(),
            source,
        }
    })?;
    validate_dump(&dump)?;
    Ok(dump)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::fs;

    fn node(type_name: &str, name: &str, version: i32) -> Value {
        json!({
            "TypeName": type_name,
            "Name": name,
            "Level": 0,
            "ByteSize": -1,
            "Index": 0,
            "Version": version,
            "TypeFlags": 0,
            "MetaFlag": 0,
            "SubNodes": []
        })
    }

    fn dump(version: &str, class_id: i32, editor: Value, release: Value) -> Value {
        json!({
            "Version": version,
            "Classes": [{
                "TypeID": class_id,
                "EditorRootNode": editor,
                "ReleaseRootNode": release
            }]
        })
    }

    #[test]
    fn resolve_defaults_to_release_tree() {
        let json = dump(
            "2022.3.0f1",
            1,
            node("EditorGameObject", "EditorBase", 3),
            node("GameObject", "Base", 5),
        );
        let registry = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&json).unwrap().as_slice(),
        )
        .unwrap();

        let tree = registry.resolve("2022.3.0f1", 1).unwrap();
        assert_eq!(tree.nodes[0].type_name, "GameObject");
        assert_eq!(tree.nodes[0].name, "Base");

        let editor_tree = registry
            .resolve_with_mode("2022.3.0f1", 1, TypeTreeSerializationMode::Editor)
            .unwrap();
        assert_eq!(editor_tree.nodes[0].type_name, "EditorGameObject");
        assert_eq!(editor_tree.nodes[0].name, "EditorBase");
    }

    #[test]
    fn single_mode_classes_only_resolve_for_their_mode() {
        let json = json!({
            "Version": "2022.3.0f1",
            "Classes": [
                {
                    "TypeID": 1,
                    "EditorRootNode": node("EditorOnly", "Base", 1),
                    "ReleaseRootNode": null
                },
                {
                    "TypeID": 2,
                    "EditorRootNode": null,
                    "ReleaseRootNode": node("ReleaseOnly", "Base", 1)
                }
            ]
        });
        let registry = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&json).unwrap().as_slice(),
        )
        .unwrap();

        assert!(registry.resolve("2022.3.0f1", 1).is_none());
        let editor_tree = registry
            .resolve_with_mode("2022.3.0f1", 1, TypeTreeSerializationMode::Editor)
            .unwrap();
        assert_eq!(editor_tree.nodes[0].type_name, "EditorOnly");

        assert!(
            registry
                .resolve_with_mode("2022.3.0f1", 2, TypeTreeSerializationMode::Editor)
                .is_none()
        );
        let release_tree = registry
            .resolve_with_mode("2022.3.0f1", 2, TypeTreeSerializationMode::Release)
            .unwrap();
        assert_eq!(release_tree.nodes[0].type_name, "ReleaseOnly");
    }

    #[test]
    fn non_exact_versions_are_rejected() {
        let root = node("GameObject", "Base", 1);
        for invalid_version in ["", "2022.*"] {
            let json = dump(invalid_version, 1, root.clone(), root.clone());
            let error = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
                serde_json::to_vec(&json).unwrap().as_slice(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("Version"));
            assert!(error.to_string().contains("exact"));
        }
    }

    #[test]
    fn load_version_reports_versions_loaded_from_reader_or_file() {
        let version = "2022.3.0f1";
        let root = node("GameObject", "Base", 1);
        let json = dump(version, 1, root.clone(), root);
        let bytes = serde_json::to_vec(&json).unwrap();

        let from_reader =
            AssetRipperTypeTreeGeneratorRegistry::new_from_reader(bytes.as_slice()).unwrap();
        assert!(from_reader.load_version(version).unwrap());

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("single.json");
        fs::write(&path, bytes).unwrap();
        let from_file = AssetRipperTypeTreeGeneratorRegistry::new_from_path(&path).unwrap();
        assert!(from_file.load_version(version).unwrap());
    }

    #[test]
    fn classes_without_either_root_are_skipped() {
        let json = dump("2022.3.0f1", 0, Value::Null, Value::Null);
        let registry = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&json).unwrap().as_slice(),
        )
        .unwrap();

        assert!(registry.resolve("2022.3.0f1", 0).is_none());
    }

    #[test]
    fn missing_required_fields_are_reported() {
        let missing_version = json!({ "Classes": [] });
        let error = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&missing_version).unwrap().as_slice(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Version"));

        let missing_type_id = json!({
            "Version": "2022.3.0f1",
            "Classes": [{
                "EditorRootNode": node("GameObject", "Base", 1),
                "ReleaseRootNode": node("GameObject", "Base", 1)
            }]
        });
        let error = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&missing_type_id).unwrap().as_slice(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("TypeID"));

        let missing_release_root = json!({
            "Version": "2022.3.0f1",
            "Classes": [{
                "TypeID": 1,
                "EditorRootNode": node("GameObject", "Base", 1)
            }]
        });
        let error = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&missing_release_root)
                .unwrap()
                .as_slice(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("ReleaseRootNode"));

        let mut missing_meta_flag = node("GameObject", "Base", 1);
        missing_meta_flag
            .as_object_mut()
            .unwrap()
            .remove("MetaFlag");
        let json = dump(
            "2022.3.0f1",
            1,
            missing_meta_flag.clone(),
            missing_meta_flag,
        );
        let error = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&json).unwrap().as_slice(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("MetaFlag"));

        let mut null_sub_nodes = node("GameObject", "Base", 1);
        null_sub_nodes["SubNodes"] = Value::Null;
        let json = dump("2022.3.0f1", 1, null_sub_nodes.clone(), null_sub_nodes);
        let error = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&json).unwrap().as_slice(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("expected a sequence"));
    }

    #[test]
    fn root_node_version_does_not_replace_type_tree_format_version() {
        let root = node("GameObject", "Base", 7);
        let json = dump("2022.3.0f1", 1, root.clone(), root);
        let registry = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&json).unwrap().as_slice(),
        )
        .unwrap();

        let tree = registry.resolve("2022.3.0f1", 1).unwrap();
        assert_eq!(tree.version, TypeTree::new().version);
        assert_eq!(tree.nodes[0].version, 7);
    }

    #[test]
    fn meta_flag_preserves_all_u32_bits() {
        let mut root = node("GameObject", "Base", 24_600);
        root["MetaFlag"] = json!(0xf000_0001_u32);
        let json = dump("2022.3.0f1", 1, root.clone(), root);
        let registry = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
            serde_json::to_vec(&json).unwrap().as_slice(),
        )
        .unwrap();

        let tree = registry.resolve("2022.3.0f1", 1).unwrap();
        assert_eq!(tree.nodes[0].meta_flags as u32, 0xf000_0001);
        assert_eq!(tree.nodes[0].version, 24_600);
    }

    #[test]
    fn node_protocol_integer_widths_are_validated() {
        for (field, value, expected_type) in [
            ("Level", json!(256), "u8"),
            ("Version", json!(65_536), "u16"),
            ("TypeFlags", json!(256), "u8"),
            ("MetaFlag", json!(u64::from(u32::MAX) + 1), "u32"),
        ] {
            let mut root = node("GameObject", "Base", 1);
            root[field] = value;
            let json = dump("2022.3.0f1", 1, root.clone(), root);

            let error = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
                serde_json::to_vec(&json).unwrap().as_slice(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(expected_type),
                "unexpected error for {field}: {error}"
            );
        }
    }

    #[test]
    fn invalid_node_semantics_include_version_class_and_field_context() {
        for (field, value) in [("TypeName", json!("")), ("ByteSize", json!(-2))] {
            let mut root = node("GameObject", "Base", 1);
            root[field] = value;
            let json = dump("2022.3.0f1", 42, root.clone(), root);

            let error = AssetRipperTypeTreeGeneratorRegistry::new_from_reader(
                serde_json::to_vec(&json).unwrap().as_slice(),
            )
            .unwrap_err();
            let message = error.to_string();
            assert!(message.contains("2022.3.0f1"), "{message}");
            assert!(message.contains("class 42"), "{message}");
            assert!(message.contains(field), "{message}");
        }
    }

    #[test]
    fn dump_directories_are_indexed_without_eagerly_parsing_files() {
        let temp = tempfile::tempdir().unwrap();
        let info_json = temp.path().join("InfoJson");
        fs::create_dir(&info_json).unwrap();
        fs::write(info_json.join("broken.json"), b"not json").unwrap();

        let valid_version = "2022.3.0f1";
        let root = node("GameObject", "Base", 1);
        let valid_dump = dump(valid_version, 1, root.clone(), root);
        fs::write(
            info_json.join(format!("{valid_version}.json")),
            serde_json::to_vec(&valid_dump).unwrap(),
        )
        .unwrap();

        let from_root = AssetRipperTypeTreeGeneratorRegistry::new_from_path(temp.path());
        assert!(from_root.is_ok());

        let from_root = from_root.unwrap();
        assert!(from_root.resolve(valid_version, 1).is_some());
        let error = from_root.load_version("broken").unwrap_err();
        assert!(error.to_string().contains("broken.json"));
        assert!(error.to_string().contains("expected ident"));

        let from_info_json = AssetRipperTypeTreeGeneratorRegistry::new_from_path(&info_json);
        assert!(from_info_json.is_ok());
        assert!(
            from_info_json
                .unwrap()
                .resolve_with_mode(valid_version, 1, TypeTreeSerializationMode::Editor)
                .is_some()
        );
    }

    #[test]
    fn dump_directory_index_is_not_recursive() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let root = node("Nested", "Base", 1);
        let json = dump("2022.3.0f1", 1, root.clone(), root);
        fs::write(
            nested.join("2022.3.0f1.json"),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();

        let error = AssetRipperTypeTreeGeneratorRegistry::new_from_path(temp.path()).unwrap_err();
        assert!(error.to_string().contains(".json"));
    }

    #[test]
    fn empty_dump_directories_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let error = AssetRipperTypeTreeGeneratorRegistry::new_from_path(temp.path()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(".json"), "{message}");
        assert!(
            message.contains(&temp.path().display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn single_file_paths_are_validated_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("invalid.json");
        fs::write(&path, br#"{"Classes": []}"#).unwrap();

        let error = AssetRipperTypeTreeGeneratorRegistry::new_from_path(&path).unwrap_err();
        assert!(error.to_string().contains("invalid.json"));
        assert!(error.to_string().contains("Version"));
    }
}
