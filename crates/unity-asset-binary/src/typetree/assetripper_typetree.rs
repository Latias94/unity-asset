//! Eager, budgeted registry for AssetRipper TypeTree `InfoJson` dumps.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Deserializer};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedJsonError, ContractJsonLimits, ContractJsonResourceModel,
    arc_value_allocation_bytes, read_contract_json, vec_allocation_bytes,
};

use crate::error::{BinaryError, Result};
use crate::typetree::{TypeTree, TypeTreeNode, TypeTreeRegistry, TypeTreeSerializationMode};

const ASSETRIPPER_JSON_RESOURCES: ContractJsonResourceModel =
    ContractJsonResourceModel::new(6, 4 * 1024, 4 * 1024, 256);
const ASSETRIPPER_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "assetripper.typetree.v1",
    128 * 1024 * 1024,
    59,
    4_000_000,
    4_000_000,
    ASSETRIPPER_JSON_RESOURCES,
);

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

fn deserialize_required_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug)]
struct ClassTypeTrees {
    class_id: i32,
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

#[derive(Debug)]
struct VersionTypeTrees {
    version: String,
    classes: Vec<ClassTypeTrees>,
}

/// Immutable registry built from one AssetRipper dump or an `InfoJson` directory.
///
/// All files are read, validated, converted, and charged to the caller's budget before the
/// constructor returns. Resolution performs only two binary searches and an `Arc` clone.
#[derive(Debug)]
pub struct AssetRipperTypeTreeRegistry {
    versions: Vec<VersionTypeTrees>,
}

impl AssetRipperTypeTreeRegistry {
    /// Reads one complete AssetRipper TypeTree JSON document.
    pub fn new_from_reader(reader: impl Read, budget: &mut AssetLoadBudget) -> Result<Self> {
        let dump = read_dump(reader, budget)?;
        let version = build_version(dump, budget)?;
        let mut versions = Vec::new();
        reserve_exact_budgeted(
            &mut versions,
            1,
            budget,
            "AssetRipper registry version table",
        )?;
        versions.push(version);
        Ok(Self { versions })
    }

    /// Loads one dump file or every immediate `.json` file in an AssetRipper dump directory.
    ///
    /// If `path/InfoJson` is a directory, it takes precedence over JSON files directly under
    /// `path`. Directory traversal is deliberately non-recursive.
    pub fn new_from_path(path: impl AsRef<Path>, budget: &mut AssetLoadBudget) -> Result<Self> {
        let path = path.as_ref();
        if fs::metadata(path)?.is_dir() {
            Self::new_from_directory(path, budget)
        } else {
            let file = File::open(path)?;
            Self::new_from_reader(file, budget).map_err(|error| with_path_context(error, path))
        }
    }

    fn new_from_directory(path: &Path, budget: &mut AssetLoadBudget) -> Result<Self> {
        let info_json = path.join("InfoJson");
        let directory = match fs::metadata(&info_json) {
            Ok(metadata) if metadata.is_dir() => info_json.as_path(),
            Ok(_) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
            Err(error) => return Err(error.into()),
        };

        let mut versions = Vec::new();
        let mut json_files = 0_u64;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            budget.consume_members(1)?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let file_path = entry.path();
            if !is_json_path(&file_path) {
                continue;
            }

            json_files = json_files
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "AssetRipper registry JSON files",
                })?;

            let file = File::open(&file_path)?;
            let dump =
                read_dump(file, budget).map_err(|error| with_path_context(error, &file_path))?;
            let file_version = file_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| {
                    BinaryError::invalid_format(format!(
                        "AssetRipper TypeTree filename {} has no UTF-8 version stem",
                        file_path.display()
                    ))
                })?;
            if dump.version != file_version {
                return Err(BinaryError::invalid_format(format!(
                    "AssetRipper TypeTree file {} declares Version {:?}, expected {:?}",
                    file_path.display(),
                    dump.version,
                    file_version
                )));
            }
            let version = build_version(dump, budget)?;
            reserve_exact_budgeted(
                &mut versions,
                1,
                budget,
                "AssetRipper registry version table",
            )?;
            versions.push(version);
        }

        if json_files == 0 {
            return Err(BinaryError::invalid_format(format!(
                "AssetRipper TypeTree directory {} contains no immediate .json files",
                directory.display()
            )));
        }

        versions.sort_unstable_by(|left, right| left.version.cmp(&right.version));
        if let Some(duplicate) = versions
            .windows(2)
            .find(|pair| pair[0].version == pair[1].version)
        {
            return Err(BinaryError::invalid_format(format!(
                "AssetRipper TypeTree directory contains duplicate Version {:?}",
                duplicate[0].version
            )));
        }

        Ok(Self { versions })
    }
}

impl TypeTreeRegistry for AssetRipperTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        self.resolve_with_mode(unity_version, class_id, TypeTreeSerializationMode::Release)
    }

    fn resolve_with_mode(
        &self,
        unity_version: &str,
        class_id: i32,
        mode: TypeTreeSerializationMode,
    ) -> Option<Arc<TypeTree>> {
        let version_index = self
            .versions
            .binary_search_by(|candidate| candidate.version.as_str().cmp(unity_version))
            .ok()?;
        let classes = &self.versions[version_index].classes;
        let class_index = classes
            .binary_search_by_key(&class_id, |candidate| candidate.class_id)
            .ok()?;
        classes[class_index].resolve(mode)
    }
}

fn read_dump(reader: impl Read, budget: &mut AssetLoadBudget) -> Result<AssetRipperDump> {
    read_contract_json(reader, budget, ASSETRIPPER_JSON_LIMITS).map_err(map_contract_error)
}

fn map_contract_error(error: BudgetedJsonError) -> BinaryError {
    match error {
        BudgetedJsonError::Io(error) => error.into(),
        BudgetedJsonError::Budget(error) => error.into(),
        BudgetedJsonError::AllocationFailed { requested } => BinaryError::memory_error(format!(
            "failed to reserve {requested} bytes for AssetRipper TypeTree JSON"
        )),
        BudgetedJsonError::Json(error) => {
            BinaryError::invalid_format(format!("invalid AssetRipper TypeTree JSON: {error}"))
        }
        error @ (BudgetedJsonError::EncodedLimitExceeded { .. }
        | BudgetedJsonError::StructureLimitExceeded { .. }) => {
            BinaryError::ResourceLimitExceeded(error.to_string())
        }
        error @ BudgetedJsonError::InvalidLimit { .. } => {
            BinaryError::invalid_data(error.to_string())
        }
    }
}

fn with_path_context(error: BinaryError, path: &Path) -> BinaryError {
    match error {
        BinaryError::InvalidFormat(message) => BinaryError::invalid_format(format!(
            "AssetRipper TypeTree file {}: {message}",
            path.display()
        )),
        error => error,
    }
}

fn build_version(
    mut dump: AssetRipperDump,
    budget: &mut AssetLoadBudget,
) -> Result<VersionTypeTrees> {
    validate_exact_version(&dump.version)?;
    dump.classes.sort_unstable_by_key(|class| class.type_id);
    if let Some(duplicate) = dump
        .classes
        .windows(2)
        .find(|pair| pair[0].type_id == pair[1].type_id)
    {
        return Err(BinaryError::invalid_format(format!(
            "AssetRipper Version {:?} contains duplicate class ID {}",
            dump.version, duplicate[0].type_id
        )));
    }

    for class in &dump.classes {
        if let Some(root) = &class.release_root_node {
            validate_node(
                &dump.version,
                class.type_id,
                "ReleaseRootNode",
                root,
                0,
                budget,
            )?;
        }
        if let Some(root) = &class.editor_root_node {
            validate_node(
                &dump.version,
                class.type_id,
                "EditorRootNode",
                root,
                0,
                budget,
            )?;
        }
    }

    let retained_classes = dump
        .classes
        .iter()
        .filter(|class| class.release_root_node.is_some() || class.editor_root_node.is_some())
        .count();
    let mut classes = Vec::new();
    reserve_exact_budgeted(
        &mut classes,
        retained_classes,
        budget,
        "AssetRipper registry class table",
    )?;
    for class in dump.classes {
        if class.release_root_node.is_none() && class.editor_root_node.is_none() {
            continue;
        }
        classes.push(ClassTypeTrees {
            class_id: class.type_id,
            release: class
                .release_root_node
                .map(|root| build_type_tree(root, budget))
                .transpose()?,
            editor: class
                .editor_root_node
                .map(|root| build_type_tree(root, budget))
                .transpose()?,
        });
    }

    Ok(VersionTypeTrees {
        version: dump.version,
        classes,
    })
}

fn validate_exact_version(version: &str) -> Result<()> {
    if version.is_empty() || version.contains('*') {
        return Err(BinaryError::invalid_format(format!(
            "AssetRipper Version {version:?} must be a non-empty exact Unity version without wildcards"
        )));
    }
    Ok(())
}

fn validate_node(
    version: &str,
    class_id: i32,
    root_name: &str,
    node: &AssetRipperNode,
    expected_level: u8,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    budget.observe_depth(u32::from(expected_level))?;
    if node.level != expected_level {
        return Err(invalid_node_field(
            version,
            class_id,
            root_name,
            "Level",
            format_args!(
                "must match nesting level {expected_level}, got {}",
                node.level
            ),
        ));
    }
    if node.type_name.is_empty() {
        return Err(invalid_node_field(
            version,
            class_id,
            root_name,
            "TypeName",
            format_args!("must not be empty"),
        ));
    }
    if node.name.is_empty() {
        return Err(invalid_node_field(
            version,
            class_id,
            root_name,
            "Name",
            format_args!("must not be empty"),
        ));
    }
    if node.byte_size < -1 {
        return Err(invalid_node_field(
            version,
            class_id,
            root_name,
            "ByteSize",
            format_args!("must be at least -1, got {}", node.byte_size),
        ));
    }

    let child_level = expected_level.checked_add(1);
    if !node.sub_nodes.is_empty() && child_level.is_none() {
        return Err(invalid_node_field(
            version,
            class_id,
            root_name,
            "SubNodes",
            format_args!("cannot exceed the u8 Level domain"),
        ));
    }
    if let Some(child_level) = child_level {
        for child in &node.sub_nodes {
            validate_node(version, class_id, root_name, child, child_level, budget)?;
        }
    }
    Ok(())
}

fn invalid_node_field(
    version: &str,
    class_id: i32,
    root_name: &str,
    field: &str,
    detail: std::fmt::Arguments<'_>,
) -> BinaryError {
    BinaryError::invalid_format(format!(
        "AssetRipper Version {version:?}, class {class_id}, {root_name}.{field} {detail}"
    ))
}

fn build_type_tree(root: AssetRipperNode, budget: &mut AssetLoadBudget) -> Result<Arc<TypeTree>> {
    let root = convert_node(root, budget)?;
    let mut tree = TypeTree::new();
    reserve_exact_budgeted(
        &mut tree.nodes,
        1,
        budget,
        "AssetRipper TypeTree root table",
    )?;
    tree.nodes.push(root);
    consume_arc_allocation::<TypeTree>(budget)?;
    Ok(Arc::new(tree))
}

fn convert_node(node: AssetRipperNode, budget: &mut AssetLoadBudget) -> Result<TypeTreeNode> {
    let AssetRipperNode {
        type_name,
        name,
        level,
        byte_size,
        index,
        version,
        type_flags,
        meta_flag,
        sub_nodes,
    } = node;

    let mut children = Vec::new();
    reserve_exact_budgeted(
        &mut children,
        sub_nodes.len(),
        budget,
        "AssetRipper TypeTree child table",
    )?;
    for child in sub_nodes {
        children.push(convert_node(child, budget)?);
    }

    Ok(TypeTreeNode {
        type_name,
        name,
        byte_size,
        variable_count: 0,
        index,
        type_flags: i32::from(type_flags),
        version: i32::from(version),
        meta_flags: i32::from_ne_bytes(meta_flag.to_ne_bytes()),
        level: i32::from(level),
        type_str_offset: 0,
        name_str_offset: 0,
        ref_type_hash: 0,
        children,
    })
}

fn reserve_exact_budgeted<T>(
    values: &mut Vec<T>,
    additional: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<()> {
    if additional == 0 {
        return Ok(());
    }
    let allocation = vec_allocation_bytes::<T>(additional)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let requested = usize::try_from(allocation)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(allocation)?;
    values
        .try_reserve_exact(additional)
        .map_err(|error| BinaryError::allocation(resource, requested, error))?;
    budget.consume_bytes(allocation)?;
    Ok(())
}

fn consume_arc_allocation<T>(budget: &mut AssetLoadBudget) -> Result<()> {
    let allocation = arc_value_allocation_bytes::<T>()
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.consume_bytes(allocation)?;
    Ok(())
}

fn is_json_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Value, json};
    use unity_asset_core::{AssetLoadLimits, BudgetError};

    use super::*;

    fn node(type_name: &str, name: &str, level: u8, version: u16) -> Value {
        json!({
            "TypeName": type_name,
            "Name": name,
            "Level": level,
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

    fn load(value: &Value, budget: &mut AssetLoadBudget) -> Result<AssetRipperTypeTreeRegistry> {
        let encoded = serde_json::to_vec(value).unwrap();
        AssetRipperTypeTreeRegistry::new_from_reader(encoded.as_slice(), budget)
    }

    #[test]
    fn release_is_default_and_editor_is_selected_explicitly() {
        let value = dump(
            "2022.3.0f1",
            1,
            node("EditorGameObject", "EditorBase", 0, 3),
            node("GameObject", "Base", 0, 5),
        );
        let registry = load(&value, &mut AssetLoadBudget::default()).unwrap();

        let release = registry.resolve("2022.3.0f1", 1).unwrap();
        assert_eq!(release.nodes[0].type_name, "GameObject");
        assert_eq!(release.nodes[0].version, 5);
        let editor = registry
            .resolve_with_mode("2022.3.0f1", 1, TypeTreeSerializationMode::Editor)
            .unwrap();
        assert_eq!(editor.nodes[0].type_name, "EditorGameObject");
        assert_eq!(editor.nodes[0].version, 3);
    }

    #[test]
    fn single_mode_classes_only_resolve_for_that_mode() {
        let value = json!({
            "Version": "2022.3.0f1",
            "Classes": [
                {
                    "TypeID": 1,
                    "EditorRootNode": node("EditorOnly", "Base", 0, 1),
                    "ReleaseRootNode": null
                },
                {
                    "TypeID": 2,
                    "EditorRootNode": null,
                    "ReleaseRootNode": node("ReleaseOnly", "Base", 0, 1)
                }
            ]
        });
        let registry = load(&value, &mut AssetLoadBudget::default()).unwrap();

        assert!(registry.resolve("2022.3.0f1", 1).is_none());
        assert!(
            registry
                .resolve_with_mode("2022.3.0f1", 1, TypeTreeSerializationMode::Editor)
                .is_some()
        );
        assert!(
            registry
                .resolve_with_mode("2022.3.0f1", 2, TypeTreeSerializationMode::Editor)
                .is_none()
        );
        assert!(registry.resolve("2022.3.0f1", 2).is_some());
    }

    #[test]
    fn node_metadata_preserves_wire_bits_without_changing_tree_format_version() {
        let mut root = node("GameObject", "Base", 0, 24_600);
        root["MetaFlag"] = json!(0xf000_0001_u32);
        let registry = load(
            &dump("2022.3.0f1", 1, root.clone(), root),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        let tree = registry.resolve("2022.3.0f1", 1).unwrap();
        assert_eq!(tree.version, TypeTree::new().version);
        assert_eq!(tree.nodes[0].version, 24_600);
        assert_eq!(tree.nodes[0].meta_flags as u32, 0xf000_0001);
    }

    #[test]
    fn exact_versions_and_node_semantics_are_validated() {
        for version in ["", "2022.*"] {
            let root = node("GameObject", "Base", 0, 1);
            let error = load(
                &dump(version, 1, root.clone(), root),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("exact"));
        }

        let mut invalid_level = node("GameObject", "Base", 0, 1);
        invalid_level["SubNodes"] = json!([node("int", "m_Value", 0, 1)]);
        let error = load(
            &dump("2022.3.0f1", 42, invalid_level.clone(), invalid_level),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("class 42"), "{message}");
        assert!(message.contains("Level"), "{message}");

        let mut invalid_size = node("GameObject", "Base", 0, 1);
        invalid_size["ByteSize"] = json!(-2);
        let error = load(
            &dump("2022.3.0f1", 42, invalid_size.clone(), invalid_size),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("ByteSize"));
    }

    #[test]
    fn missing_null_and_unknown_fields_follow_the_pascal_case_contract() {
        let missing_release = json!({
            "Version": "2022.3.0f1",
            "Classes": [{
                "TypeID": 1,
                "EditorRootNode": null
            }]
        });
        assert!(
            load(&missing_release, &mut AssetLoadBudget::default())
                .unwrap_err()
                .to_string()
                .contains("ReleaseRootNode")
        );

        let no_trees = dump("2022.3.0f1", 1, Value::Null, Value::Null);
        let registry = load(&no_trees, &mut AssetLoadBudget::default()).unwrap();
        assert!(registry.resolve("2022.3.0f1", 1).is_none());

        let mut unknown = no_trees;
        unknown["Unexpected"] = json!(true);
        load(&unknown, &mut AssetLoadBudget::default()).unwrap();
    }

    #[test]
    fn duplicate_versions_and_classes_are_rejected() {
        let root = node("GameObject", "Base", 0, 1);
        let duplicate_class = json!({
            "Version": "2022.3.0f1",
            "Classes": [
                {
                    "TypeID": 1,
                    "EditorRootNode": null,
                    "ReleaseRootNode": root.clone()
                },
                {
                    "TypeID": 1,
                    "EditorRootNode": null,
                    "ReleaseRootNode": root.clone()
                }
            ]
        });
        assert!(
            load(&duplicate_class, &mut AssetLoadBudget::default())
                .unwrap_err()
                .to_string()
                .contains("duplicate class ID")
        );

        let temp = tempfile::tempdir().unwrap();
        let encoded = serde_json::to_vec(&dump("2022.3.0f1", 1, Value::Null, root)).unwrap();
        fs::write(temp.path().join("wrong-name.json"), &encoded).unwrap();
        let error = AssetRipperTypeTreeRegistry::new_from_path(
            temp.path(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("expected \"wrong-name\""));
    }

    #[test]
    fn directory_prefers_info_json_and_is_non_recursive() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("ignored.json"), b"not json").unwrap();
        let info_json = temp.path().join("InfoJson");
        fs::create_dir(&info_json).unwrap();
        let nested = info_json.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("ignored.json"), b"not json").unwrap();

        let root = node("GameObject", "Base", 0, 1);
        fs::write(
            info_json.join("2022.3.0f1.json"),
            serde_json::to_vec(&dump("2022.3.0f1", 1, root.clone(), root)).unwrap(),
        )
        .unwrap();

        let registry = AssetRipperTypeTreeRegistry::new_from_path(
            temp.path(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert!(registry.resolve("2022.3.0f1", 1).is_some());
    }

    #[test]
    fn directory_parsing_is_eager_and_empty_directories_are_rejected() {
        let broken = tempfile::tempdir().unwrap();
        fs::write(broken.path().join("broken.json"), b"not json").unwrap();
        let error = AssetRipperTypeTreeRegistry::new_from_path(
            broken.path(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("broken.json"));

        let empty = tempfile::tempdir().unwrap();
        let error = AssetRipperTypeTreeRegistry::new_from_path(
            empty.path(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no immediate .json files"));
    }

    #[test]
    fn single_file_paths_are_loaded_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("single.json");
        let root = node("GameObject", "Base", 0, 1);
        fs::write(
            &path,
            serde_json::to_vec(&dump("2022.3.0f1", 1, root.clone(), root)).unwrap(),
        )
        .unwrap();

        let registry =
            AssetRipperTypeTreeRegistry::new_from_path(&path, &mut AssetLoadBudget::default())
                .unwrap();
        fs::remove_file(&path).unwrap();

        let first = registry.resolve("2022.3.0f1", 1).unwrap();
        let second = registry.resolve("2022.3.0f1", 1).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn caller_owned_budget_has_an_exact_one_short_boundary() {
        let root = node("GameObject", "Base", 0, 1);
        let value = dump("2022.3.0f1", 1, root.clone(), root);
        let encoded = serde_json::to_vec(&value).unwrap();

        let mut measured = AssetLoadBudget::default();
        AssetRipperTypeTreeRegistry::new_from_reader(encoded.as_slice(), &mut measured).unwrap();
        let exact_bytes = measured.usage().bytes;

        let limits = AssetLoadLimits {
            max_bytes: exact_bytes,
            ..AssetLoadLimits::default()
        };
        let mut exact = AssetLoadBudget::new(limits).unwrap();
        AssetRipperTypeTreeRegistry::new_from_reader(encoded.as_slice(), &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, exact_bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            AssetRipperTypeTreeRegistry::new_from_reader(encoded.as_slice(), &mut one_short),
            Err(BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn resolution_does_not_consume_caller_budget() {
        let root = node("GameObject", "Base", 0, 1);
        let value = dump("2022.3.0f1", 1, root.clone(), root);
        let mut budget = AssetLoadBudget::default();
        let registry = load(&value, &mut budget).unwrap();
        let before = budget.usage();

        let first = registry.resolve("2022.3.0f1", 1).unwrap();
        let second = registry
            .resolve_with_mode("2022.3.0f1", 1, TypeTreeSerializationMode::Editor)
            .unwrap();

        assert_eq!(budget.usage(), before);
        assert_eq!(first.nodes[0].type_name, "GameObject");
        assert_eq!(second.nodes[0].type_name, "GameObject");
    }
}
