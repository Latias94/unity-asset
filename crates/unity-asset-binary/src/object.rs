//! Unity object representation and helpers.

use crate::asset::{ObjectInfo, SerializedFile, SerializedType};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
use crate::typetree::{
    PPtrScanResult, TypeTree, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeParseOutput,
    TypeTreeParseWarning, TypeTreeSchema, TypeTreeTraversalStats,
};
use crate::unity_objects::{GameObject, Transform};
use std::fmt::Write as _;
use std::sync::Arc;
use unity_asset_core::{AssetLoadBudget, UnityClass, UnityValue};

/// A lightweight reference to a binary object within a [`SerializedFile`].
///
/// This is conceptually similar to UnityPy's `ObjectReader`: it carries just enough context
/// (file + object metadata) to parse the object on-demand.
#[derive(Debug, Clone, Copy)]
pub struct ObjectHandle<'a> {
    file: &'a SerializedFile,
    info: &'a ObjectInfo,
}

impl<'a> ObjectHandle<'a> {
    pub fn new(file: &'a SerializedFile, info: &'a ObjectInfo) -> Self {
        Self { file, info }
    }

    pub fn file(&self) -> &'a SerializedFile {
        self.file
    }

    pub fn info(&self) -> &'a ObjectInfo {
        self.info
    }

    pub fn path_id(&self) -> i64 {
        self.info.path_id()
    }

    pub fn class_id(&self) -> i32 {
        self.info.class_id()
    }

    pub fn byte_start(&self) -> u64 {
        self.info.byte_start()
    }

    pub fn byte_size(&self) -> u32 {
        self.info.byte_size()
    }

    /// Get the raw bytes for this object (preloaded if available, otherwise sliced from the file).
    pub fn raw_data(&self) -> Result<&'a [u8]> {
        if let Some(data) = self.info.loaded_data() {
            return Ok(data);
        }
        self.file.object_bytes(self.info)
    }

    /// Parse this object into an owned [`UnityObject`] under a caller-owned resource budget.
    pub fn read(&self, budget: &mut AssetLoadBudget) -> Result<UnityObject> {
        UnityObject::from_serialized_file(self.file, self.info, budget)
    }

    pub fn read_with_options(
        &self,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<UnityObject> {
        UnityObject::from_serialized_file_with_options(self.file, self.info, budget, options)
    }

    /// Returns the canonical TypeTree schema selected for this object.
    ///
    /// Schemas backed by this file's SerializedType table are shared across every handle for the
    /// same type. External registry results are compiled per lookup because registries may change
    /// the returned tree without exposing a revision identity.
    pub fn schema(&self, budget: &mut AssetLoadBudget) -> Result<Option<TypeTreeSchema>> {
        let Some(source) = type_tree_for_object(self.file, self.info) else {
            return Ok(None);
        };
        match source {
            TypeTreeSource::Internal(type_index) => self
                .file
                .cached_internal_schema(type_index, budget)
                .map(Some),
            TypeTreeSource::External(tree) => {
                self.file.compile_schema(tree.as_ref(), budget).map(Some)
            }
        }
    }

    /// Peek the object's name (`m_Name`/`name`) without parsing the full TypeTree.
    ///
    /// This mirrors UnityPy's `ObjectReader.peek_name()` behavior by parsing only a prefix of the
    /// root TypeTree until the name field, when possible.
    pub fn peek_name(&self, budget: &mut AssetLoadBudget) -> Result<Option<String>> {
        self.peek_name_with_options(
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Lenient,
            },
        )
    }

    pub fn peek_name_with_options(
        &self,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Option<String>> {
        let Some(schema) = self.schema(budget)? else {
            return Ok(None);
        };
        let Some((prefix_len, field)) = name_peek_prefix(&schema) else {
            return Ok(None);
        };

        let bytes = self.raw_data()?;
        let mut reader = BinaryReader::new(bytes, self.file.header.byte_order());
        let mut out = schema.read_object_prefix(&mut reader, budget, options, prefix_len)?;

        match out.properties.shift_remove(field) {
            Some(UnityValue::String(value)) => Ok(Some(value)),
            _ => Ok(None),
        }
    }

    /// Scan TypeTree-based object bytes and collect `PPtr` references (`fileID`, `pathID`) without
    /// allocating a full parsed `UnityValue` tree.
    pub fn scan_pptrs(&self, budget: &mut AssetLoadBudget) -> Result<Option<PPtrScanResult>> {
        let Some(schema) = self.schema(budget)? else {
            return Ok(None);
        };
        let bytes = self.raw_data()?;
        let mut reader = BinaryReader::new(bytes, self.file.header.byte_order());
        Ok(Some(schema.scan_pptrs(&mut reader, budget)?))
    }
}

impl SerializedFile {
    /// Iterate all objects as lightweight handles.
    pub fn object_handles(&self) -> impl Iterator<Item = ObjectHandle<'_>> {
        self.objects()
            .iter()
            .map(|info| ObjectHandle::new(self, info))
    }

    /// Find an object by `path_id` and return a lightweight handle.
    pub fn find_object_handle(&self, path_id: i64) -> Option<ObjectHandle<'_>> {
        self.find_object(path_id)
            .map(|info| ObjectHandle::new(self, info))
    }
}

#[derive(Debug, Clone)]
enum ObjectBytes {
    Empty,
    Inline(Vec<u8>),
    Shared {
        data: SharedBytes,
        start: usize,
        end: usize,
    },
}

impl ObjectBytes {
    fn copy_from_slice(bytes: &[u8], budget: &mut AssetLoadBudget) -> Result<Self> {
        let amount = u64::try_from(bytes.len()).map_err(|_| {
            BinaryError::memory_error("Owned object payload length does not fit in u64")
        })?;
        budget.check_bytes(amount)?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(bytes.len()).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {} bytes for an owned object payload: {error}",
                bytes.len()
            ))
        })?;
        copy.extend_from_slice(bytes);
        budget.consume_bytes(amount)?;
        Ok(Self::Inline(copy))
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            ObjectBytes::Empty => &[],
            ObjectBytes::Inline(bytes) => bytes.as_slice(),
            ObjectBytes::Shared { data, start, end } => &data.as_bytes()[*start..*end],
        }
    }
}

/// A parsed Unity object.
///
/// This is an owned wrapper which carries:
/// - the raw `ObjectInfo` (from `asset` module)
/// - the parsed `UnityClass` properties (best-effort)
#[derive(Debug, Clone)]
pub struct UnityObject {
    pub info: ObjectInfo,
    pub class: UnityClass,
    byte_order: ByteOrder,
    raw: ObjectBytes,
    typetree_warnings: Vec<TypeTreeParseWarning>,
    typetree_stats: TypeTreeTraversalStats,
}

impl UnityObject {
    /// Create a UnityObject from an already-parsed UnityClass (used by tests and higher-level code).
    pub fn from_info_and_class(info: ObjectInfo, class: UnityClass) -> Self {
        Self {
            byte_order: ByteOrder::Little,
            info,
            class,
            raw: ObjectBytes::Empty,
            typetree_warnings: Vec::new(),
            typetree_stats: TypeTreeTraversalStats::default(),
        }
    }

    /// Create a UnityObject from raw bytes without TypeTree information.
    ///
    /// For large objects, this intentionally avoids expanding all bytes into a `UnityValue::Array`
    /// to reduce memory pressure and parsing time; use `raw_data()` instead.
    pub fn from_raw(class_id: i32, path_id: i64, data: Vec<u8>) -> Result<Self> {
        let byte_size = u32::try_from(data.len()).map_err(|_| {
            BinaryError::invalid_data(format!(
                "Raw object payload is too large for the u32 wire size: {} bytes",
                data.len()
            ))
        })?;
        let info = ObjectInfo::for_standalone_class(path_id, 0, byte_size, class_id)?;
        let raw = ObjectBytes::Inline(data);
        let class = UnityClass::new(class_id, class_name_from_id(class_id), path_id.to_string());
        Ok(Self {
            info,
            class,
            byte_order: ByteOrder::Little,
            raw,
            typetree_warnings: Vec::new(),
            typetree_stats: TypeTreeTraversalStats::default(),
        })
    }

    /// Create a UnityObject from a SerializedFile + ObjectInfo, using TypeTree when available.
    pub fn from_serialized_file(
        file: &SerializedFile,
        info: &ObjectInfo,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        Self::from_serialized_file_with_options(file, info, budget, TypeTreeParseOptions::default())
    }

    pub fn from_serialized_file_with_options(
        file: &SerializedFile,
        info: &ObjectInfo,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<Self> {
        let class_id = info.class_id();
        let byte_order = file.header.byte_order();
        let anchor = anchor_from_path_id(info.path_id(), budget)?;
        let class_name = class_name_from_id_with_budget(class_id, budget)?;
        let schema = ObjectHandle::new(file, info).schema(budget)?;
        let (class, warnings, stats) = match schema {
            Some(schema) => {
                let out = parse_object_data(file, info, byte_order, &schema, budget, options)?;
                (
                    UnityClass::with_properties(class_id, class_name, anchor, out.properties),
                    out.warnings,
                    out.stats,
                )
            }
            _ => (
                UnityClass::new(class_id, class_name, anchor),
                Vec::new(),
                TypeTreeTraversalStats::default(),
            ),
        };
        let raw = owned_object_bytes(file, info, budget)?;

        Ok(Self {
            info: info.clone_without_loaded_data(),
            class,
            byte_order,
            raw,
            typetree_warnings: warnings,
            typetree_stats: stats,
        })
    }

    pub fn path_id(&self) -> i64 {
        self.info.path_id()
    }

    pub fn class_id(&self) -> i32 {
        self.info.class_id()
    }

    pub fn class_name(&self) -> &str {
        &self.class.class_name
    }

    pub fn name(&self) -> Option<String> {
        self.class.get("m_Name").and_then(|v| match v {
            UnityValue::String(s) => Some(s.clone()),
            _ => None,
        })
    }

    pub fn get(&self, key: &str) -> Option<&UnityValue> {
        self.class.get(key)
    }

    pub fn set(&mut self, key: String, value: UnityValue) {
        self.class.set(key, value);
    }

    pub fn has_property(&self, key: &str) -> bool {
        self.class.has_property(key)
    }

    pub fn property_names(&self) -> Vec<&String> {
        self.class.properties().keys().collect()
    }

    pub fn as_unity_class(&self) -> &UnityClass {
        &self.class
    }

    pub fn as_unity_class_mut(&mut self) -> &mut UnityClass {
        &mut self.class
    }

    pub fn as_gameobject(&self) -> Result<GameObject> {
        if self.class_id() != 1 {
            return Err(BinaryError::invalid_data(format!(
                "Object is not a GameObject (class_id: {})",
                self.class_id()
            )));
        }
        GameObject::from_typetree(self.class.properties())
    }

    pub fn as_transform(&self) -> Result<Transform> {
        if self.class_id() != 4 {
            return Err(BinaryError::invalid_data(format!(
                "Object is not a Transform (class_id: {})",
                self.class_id()
            )));
        }
        Transform::from_typetree(self.class.properties())
    }

    pub fn is_gameobject(&self) -> bool {
        self.class_id() == 1
    }

    pub fn is_transform(&self) -> bool {
        self.class_id() == 4
    }

    pub fn describe(&self) -> String {
        let name = self.name().unwrap_or_else(|| "<unnamed>".to_string());
        format!(
            "{} '{}' (ID:{}, PathID:{})",
            self.class_name(),
            name,
            self.class_id(),
            self.path_id()
        )
    }

    pub fn raw_data(&self) -> &[u8] {
        self.raw.as_slice()
    }

    pub fn typetree_warnings(&self) -> &[TypeTreeParseWarning] {
        &self.typetree_warnings
    }

    pub fn typetree_stats(&self) -> TypeTreeTraversalStats {
        self.typetree_stats
    }

    pub fn byte_size(&self) -> u32 {
        self.info.byte_size()
    }

    pub fn byte_start(&self) -> u64 {
        self.info.byte_start()
    }

    pub fn byte_order(&self) -> ByteOrder {
        self.byte_order
    }
}

fn class_name_from_id(class_id: i32) -> String {
    unity_asset_core::get_class_name(class_id).unwrap_or_else(|| format!("Class_{}", class_id))
}

fn class_name_from_id_with_budget(class_id: i32, budget: &mut AssetLoadBudget) -> Result<String> {
    match unity_asset_core::get_class_name_str(class_id) {
        Some(name) => copy_string_with_budget(name, budget, "Unity class name"),
        None => {
            const MAX_CLASS_NAME_LEN: usize = "Class_".len() + 11;
            let mut name = string_with_capacity(MAX_CLASS_NAME_LEN, budget, "Unity class name")?;
            write!(&mut name, "Class_{class_id}").map_err(|_| {
                BinaryError::memory_error("Failed to format fallback Unity class name")
            })?;
            Ok(name)
        }
    }
}

fn anchor_from_path_id(path_id: i64, budget: &mut AssetLoadBudget) -> Result<String> {
    const MAX_PATH_ID_LEN: usize = 20;
    let mut anchor = string_with_capacity(MAX_PATH_ID_LEN, budget, "Unity object anchor")?;
    write!(&mut anchor, "{path_id}")
        .map_err(|_| BinaryError::memory_error("Failed to format Unity object anchor"))?;
    Ok(anchor)
}

fn copy_string_with_budget(
    value: &str,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<String> {
    let mut copy = string_with_capacity(value.len(), budget, label)?;
    copy.push_str(value);
    Ok(copy)
}

fn string_with_capacity(
    capacity: usize,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<String> {
    let amount = u64::try_from(capacity)
        .map_err(|_| BinaryError::memory_error(format!("{label} capacity does not fit in u64")))?;
    budget.check_bytes(amount)?;
    let mut value = String::new();
    value.try_reserve_exact(capacity).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {capacity} bytes for {label}: {error}"
        ))
    })?;
    budget.consume_bytes(amount)?;
    Ok(value)
}

enum TypeTreeSource {
    Internal(usize),
    External(Arc<TypeTree>),
}

fn type_tree_for_object(file: &SerializedFile, info: &ObjectInfo) -> Option<TypeTreeSource> {
    fn from_internal<'a>(
        file: &'a SerializedFile,
        info: &ObjectInfo,
    ) -> Option<(usize, &'a SerializedType)> {
        if let Some(index) = info.serialized_type_index() {
            let index = usize::try_from(index).ok()?;
            return file
                .types()
                .get(index)
                .map(|serialized_type| (index, serialized_type));
        }
        file.types()
            .iter()
            .enumerate()
            .find(|(_, serialized_type)| serialized_type.class_id == info.class_id())
    }

    if file.type_tree_enabled()
        && let Some((type_index, typ)) = from_internal(file, info)
        && !typ.type_tree.is_empty()
    {
        return Some(TypeTreeSource::Internal(type_index));
    }

    // Best-effort fallback: stripped files can supply a registry externally.
    // We also allow this fallback even when `enable_type_tree = true` but the internal entry is missing/empty.
    file.type_tree_registry().and_then(|r| {
        if let Some((_, typ)) = from_internal(file, info)
            && typ.is_script_type()
            && typ.script_id != [0u8; 16]
            && let Some(tree) = r.resolve_script(&file.unity_version, typ.class_id, typ.script_id)
        {
            return Some(TypeTreeSource::External(tree));
        }

        r.resolve(&file.unity_version, info.class_id())
            .map(TypeTreeSource::External)
    })
}

fn object_bytes<'a>(file: &'a SerializedFile, info: &'a ObjectInfo) -> Result<&'a [u8]> {
    if let Some(data) = info.loaded_data() {
        return Ok(data);
    }
    file.object_bytes(info)
}

fn owned_object_bytes(
    file: &SerializedFile,
    info: &ObjectInfo,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectBytes> {
    match info.loaded_data() {
        Some(data) => ObjectBytes::copy_from_slice(data, budget),
        None => {
            let (start, end) = object_range(file, info)?;
            let base = file.data_base_offset();
            Ok(ObjectBytes::Shared {
                data: file.data_shared(),
                start: base + start,
                end: base + end,
            })
        }
    }
}

fn object_range(file: &SerializedFile, info: &ObjectInfo) -> Result<(usize, usize)> {
    let start: usize = info.byte_start().try_into().map_err(|_| {
        BinaryError::invalid_data(format!("Object byte_start overflow: {}", info.byte_start()))
    })?;
    let byte_end = info.byte_end()?;
    let end: usize = byte_end
        .try_into()
        .map_err(|_| BinaryError::invalid_data(format!("Object byte_end overflow: {byte_end}")))?;
    if end > file.data().len() {
        return Err(BinaryError::invalid_data(format!(
            "Object data out of bounds (path_id={}, start={}, size={}, file_len={})",
            info.path_id(),
            start,
            info.byte_size(),
            file.data().len()
        )));
    }
    Ok((start, end))
}

fn parse_object_data(
    file: &SerializedFile,
    info: &ObjectInfo,
    byte_order: ByteOrder,
    schema: &TypeTreeSchema,
    budget: &mut AssetLoadBudget,
    options: TypeTreeParseOptions,
) -> Result<TypeTreeParseOutput> {
    let bytes = object_bytes(file, info)?;
    let mut reader = BinaryReader::new(bytes, byte_order);
    schema.read_object(&mut reader, budget, options)
}

fn name_peek_prefix(schema: &TypeTreeSchema) -> Option<(usize, &str)> {
    schema
        .root()
        .children()
        .enumerate()
        .find(|(_, child)| child.name() == "m_Name" || child.name() == "name")
        .map(|(index, child)| (index + 1, child.name()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{ObjectMetadata, ObjectTypeReference, SerializedFileParser, SerializedType};
    use crate::typetree::{TypeTree, TypeTreeNode, TypeTreeRegistry};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use unity_asset_core::AssetLoadLimits;

    const V22_FIXTURE: &[u8] = include_bytes!(
        "../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin"
    );

    fn read(handle: ObjectHandle<'_>) -> UnityObject {
        let mut budget = AssetLoadBudget::default();
        handle.read(&mut budget).unwrap()
    }

    #[test]
    fn loaded_payload_state_drives_handles_and_owned_objects() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        // This wire fixture intentionally uses a scalar root to exercise SerializedFile state,
        // not object materialization. Keep this test on the raw-object path.
        file.set_type_tree_enabled(false);
        let path_id = file.objects()[0].path_id();
        let original = file
            .find_object_handle(path_id)
            .unwrap()
            .raw_data()
            .unwrap()
            .to_vec();
        assert!(!original.is_empty());

        file.find_object_mut(path_id).unwrap().set_data(Vec::new());
        let handle = file.find_object_handle(path_id).unwrap();
        assert!(handle.raw_data().unwrap().is_empty());
        assert!(read(handle).raw_data().is_empty());

        file.find_object_mut(path_id)
            .unwrap()
            .set_data(vec![0xD0, 0xAD]);
        let handle = file.find_object_handle(path_id).unwrap();
        assert_eq!(handle.raw_data().unwrap(), [0xD0, 0xAD]);
        assert_eq!(read(handle).raw_data(), [0xD0, 0xAD]);

        file.find_object_mut(path_id).unwrap().clear_data();
        assert_eq!(
            file.find_object_handle(path_id)
                .unwrap()
                .raw_data()
                .unwrap(),
            original
        );
    }

    #[test]
    fn raw_objects_expose_bytes_without_synthetic_properties() {
        let object = UnityObject::from_raw(1, 7, vec![0xAA, 0xBB]).unwrap();

        assert_eq!(object.raw_data(), [0xAA, 0xBB]);
        assert!(object.class.properties().is_empty());
        assert_eq!(object.typetree_stats(), TypeTreeTraversalStats::default());
    }

    #[test]
    fn owned_payload_copy_checks_then_charges_the_budget() {
        let limits = AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        };
        let mut short = AssetLoadBudget::new(limits).unwrap();
        let error = ObjectBytes::copy_from_slice(&[1, 2], &mut short).unwrap_err();
        assert!(matches!(error, BinaryError::Budget(_)));
        assert_eq!(short.usage().bytes, 0);

        let limits = AssetLoadLimits {
            max_bytes: 2,
            ..AssetLoadLimits::default()
        };
        let mut exact = AssetLoadBudget::new(limits).unwrap();
        let copied = ObjectBytes::copy_from_slice(&[1, 2], &mut exact).unwrap();
        assert_eq!(copied.as_slice(), [1, 2]);
        assert_eq!(exact.usage().bytes, 2);
    }

    #[test]
    fn object_handles_share_one_budgeted_canonical_schema() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let info = &file.objects()[0];
        let first_handle = ObjectHandle::new(&file, info);
        let second_handle = ObjectHandle::new(&file, info);
        let mut budget = AssetLoadBudget::default();

        let first = first_handle
            .schema(&mut budget)
            .unwrap()
            .expect("fixture object should have an internal TypeTree");
        let after_first = budget.usage();
        let second = second_handle
            .schema(&mut budget)
            .unwrap()
            .expect("fixture object should reuse its internal TypeTree schema");

        assert_eq!(first.root(), second.root());
        assert_eq!(budget.usage(), after_first);
    }

    #[test]
    fn ordinary_schema_ignores_an_unrelated_malformed_managed_catalog() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut malformed_tree = TypeTree::new();
        malformed_tree.add_node(TypeTreeNode::with_info(
            "BrokenManagedType".to_owned(),
            "BrokenManagedType".to_owned(),
            -1,
        ));
        let mut malformed = SerializedType::new(114);
        malformed.type_tree = malformed_tree;
        *file.ref_types_mut() = vec![malformed];

        let handle = file.object_handles().next().unwrap();
        let schema = handle.schema(&mut AssetLoadBudget::default()).unwrap();

        assert!(schema.is_some());
    }

    #[test]
    fn serialized_type_indices_bind_cache_keys_to_file_owned_trees() {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        fn object_tree(root_name: &str) -> TypeTree {
            let mut managed_type = node("ReferencedObjectType", "type");
            managed_type.children = vec![
                node("string", "class"),
                node("string", "ns"),
                node("string", "asm"),
            ];
            let mut referenced = node("ReferencedObject", "m_Reference");
            referenced.children = vec![managed_type, node("ReferencedObjectData", "data")];
            let mut root = node(root_name, root_name);
            root.children.push(referenced);
            let mut tree = TypeTree::new();
            tree.add_node(root);
            tree
        }

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut first_type = SerializedType::new(1);
        first_type.type_tree = object_tree("FirstRoot");
        let mut second_type = SerializedType::new(1);
        second_type.type_tree = object_tree("SecondRoot");
        *file.types_mut() = vec![first_type, second_type];

        let mut referenced_root = node("ManagedRoot", "ManagedRoot");
        referenced_root.children.push(node("int", "m_Value"));
        let mut referenced_tree = TypeTree::new();
        referenced_tree.add_node(referenced_root);
        let mut referenced_type = SerializedType::new(114);
        referenced_type.class_name = "ManagedRoot".to_owned();
        referenced_type.namespace = "Tests".to_owned();
        referenced_type.assembly_name = "Tests".to_owned();
        referenced_type.type_tree = referenced_tree;
        *file.ref_types_mut() = vec![referenced_type];

        let first_info = ObjectInfo::from_wire(
            1,
            0,
            0,
            ObjectTypeReference::SerializedTypeIndex { index: 0 },
            1,
            Some(0),
            ObjectMetadata::None,
        );
        let second_info = ObjectInfo::from_wire(
            2,
            0,
            0,
            ObjectTypeReference::SerializedTypeIndex { index: 1 },
            1,
            Some(1),
            ObjectMetadata::None,
        );
        let mut budget = AssetLoadBudget::default();

        assert!(matches!(
            type_tree_for_object(&file, &first_info),
            Some(TypeTreeSource::Internal(0))
        ));
        assert!(matches!(
            type_tree_for_object(&file, &second_info),
            Some(TypeTreeSource::Internal(1))
        ));

        let first_schema = ObjectHandle::new(&file, &first_info)
            .schema(&mut budget)
            .unwrap()
            .unwrap();
        let after_first = budget.usage().entries;
        let second_schema = ObjectHandle::new(&file, &second_info)
            .schema(&mut budget)
            .unwrap()
            .unwrap();

        assert_eq!(first_schema.root().type_name(), "FirstRoot");
        assert_eq!(second_schema.root().type_name(), "SecondRoot");
        assert_ne!(first_schema.root(), second_schema.root());
        assert_eq!(budget.usage().entries - after_first, 7);

        let after_both = budget.usage();
        assert_eq!(
            ObjectHandle::new(&file, &first_info)
                .schema(&mut budget)
                .unwrap()
                .unwrap()
                .root()
                .type_name(),
            "FirstRoot"
        );
        assert_eq!(
            ObjectHandle::new(&file, &second_info)
                .schema(&mut budget)
                .unwrap()
                .unwrap()
                .root()
                .type_name(),
            "SecondRoot"
        );
        assert_eq!(budget.usage(), after_both);
    }

    #[test]
    fn mutating_the_type_table_invalidates_cached_object_schemas() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let first = file
            .object_handles()
            .next()
            .unwrap()
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();

        let mut replacement_root = TypeTreeNode::with_info(
            "ReplacementRoot".to_owned(),
            "ReplacementRoot".to_owned(),
            -1,
        );
        replacement_root.children.push(TypeTreeNode::with_info(
            "int".to_owned(),
            "m_Value".to_owned(),
            -1,
        ));
        let mut replacement = TypeTree::new();
        replacement.add_node(replacement_root);
        file.types_mut()[0].type_tree = replacement;

        let second = file
            .object_handles()
            .next()
            .unwrap()
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();
        assert_eq!(second.root().type_name(), "ReplacementRoot");
        assert_ne!(first.root(), second.root());
    }

    #[test]
    fn mutating_ref_types_invalidates_cached_managed_catalogs() {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        fn managed_tree(root_name: &str) -> TypeTree {
            let mut root = node(root_name, root_name);
            root.children.push(node("int", "m_Value"));
            let mut tree = TypeTree::new();
            tree.add_node(root);
            tree
        }

        let mut managed_type = node("ReferencedObjectType", "type");
        managed_type.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Reference");
        referenced.children = vec![managed_type, node("ReferencedObjectData", "data")];
        let mut object_root = node("ObjectRoot", "ObjectRoot");
        object_root.children.push(referenced);
        let mut object_tree = TypeTree::new();
        object_tree.add_node(object_root);

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        file.types_mut()[0].type_tree = object_tree;
        let mut ref_type = SerializedType::new(114);
        ref_type.class_name = "Managed".to_owned();
        ref_type.namespace = "Tests".to_owned();
        ref_type.assembly_name = "Tests".to_owned();
        ref_type.type_tree = managed_tree("FirstManaged");
        *file.ref_types_mut() = vec![ref_type];

        let first = file
            .object_handles()
            .next()
            .unwrap()
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            first
                .resolve_managed_root("Managed", "Tests", "Tests")
                .unwrap()
                .type_name(),
            "FirstManaged"
        );

        file.ref_types_mut()[0].type_tree = managed_tree("SecondManaged");
        let second = file
            .object_handles()
            .next()
            .unwrap()
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            second
                .resolve_managed_root("Managed", "Tests", "Tests")
                .unwrap()
                .type_name(),
            "SecondManaged"
        );
        assert_ne!(first.root(), second.root());
    }

    #[test]
    fn schema_and_catalog_resource_failures_can_be_retried() {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        let mut managed_type = node("ReferencedObjectType", "type");
        managed_type.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Reference");
        referenced.children = vec![managed_type, node("ReferencedObjectData", "data")];
        let mut root = node("Root", "Root");
        root.children.push(referenced);
        let mut object_tree = TypeTree::new();
        object_tree.add_node(root);

        let mut ref_root = node("Managed", "Managed");
        ref_root.children.push(node("int", "m_Value"));
        let mut ref_tree = TypeTree::new();
        ref_tree.add_node(ref_root);

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut object_type = SerializedType::new(1);
        object_type.type_tree = object_tree;
        *file.types_mut() = vec![object_type];
        let mut ref_type = SerializedType::new(114);
        ref_type.class_name = "Managed".to_owned();
        ref_type.namespace = "Tests".to_owned();
        ref_type.assembly_name = "Tests".to_owned();
        ref_type.type_tree = ref_tree;
        *file.ref_types_mut() = vec![ref_type];
        let info = ObjectInfo::for_standalone_class(1, 0, 0, 1).unwrap();
        let handle = ObjectHandle::new(&file, &info);

        let mut constrained = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 7,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            handle.schema(&mut constrained),
            Err(BinaryError::Budget(_))
        ));

        let schema = handle
            .schema(&mut AssetLoadBudget::default())
            .unwrap()
            .expect("a larger budget should retry both schema and catalog compilation");
        assert_eq!(schema.root().type_name(), "Root");
    }

    #[test]
    fn malformed_managed_catalog_failure_is_negative_cached() {
        fn node(type_name: &str, name: &str) -> TypeTreeNode {
            TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
        }

        let mut managed_type = node("ReferencedObjectType", "type");
        managed_type.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Reference");
        referenced.children = vec![managed_type, node("ReferencedObjectData", "data")];
        let mut object_root = node("Root", "Root");
        object_root.children.push(referenced);
        let mut object_tree = TypeTree::new();
        object_tree.add_node(object_root);

        let mut managed_tree = TypeTree::new();
        managed_tree.add_node(node("Managed", "Managed"));

        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        file.types_mut()[0].type_tree = object_tree;
        let mut malformed = SerializedType::new(114);
        malformed.type_tree = managed_tree;
        *file.ref_types_mut() = vec![malformed];

        let handle = file.object_handles().next().unwrap();
        let mut budget = AssetLoadBudget::default();
        let first = handle.schema(&mut budget).unwrap_err();
        assert!(first.to_string().contains("no class name"));
        let after_first = budget.usage();

        let second = handle.schema(&mut budget).unwrap_err();
        assert!(second.to_string().contains("no class name"));
        assert_eq!(budget.usage(), after_first);
    }

    #[derive(Debug)]
    struct ChangingRegistry {
        calls: AtomicUsize,
    }

    impl TypeTreeRegistry for ChangingRegistry {
        fn resolve(&self, _unity_version: &str, _class_id: i32) -> Option<Arc<TypeTree>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let name = if call == 0 {
                "FirstExternal"
            } else {
                "SecondExternal"
            };
            let mut root = TypeTreeNode::with_info(name.to_owned(), name.to_owned(), -1);
            root.children.push(TypeTreeNode::with_info(
                "int".to_owned(),
                "m_Value".to_owned(),
                -1,
            ));
            let mut tree = TypeTree::new();
            tree.add_node(root);
            Some(Arc::new(tree))
        }
    }

    #[test]
    fn external_registry_results_are_not_cached_without_a_stable_identity() {
        let mut file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        file.set_type_tree_enabled(false);
        let registry = Arc::new(ChangingRegistry {
            calls: AtomicUsize::new(0),
        });
        file.set_type_tree_registry(Some(registry.clone()));
        let handle = file.object_handles().next().unwrap();
        let mut budget = AssetLoadBudget::default();

        let first = handle.schema(&mut budget).unwrap().unwrap();
        let second = handle.schema(&mut budget).unwrap().unwrap();

        assert_eq!(first.root().type_name(), "FirstExternal");
        assert_eq!(second.root().type_name(), "SecondExternal");
        assert_ne!(first.root(), second.root());
        assert_eq!(registry.calls.load(Ordering::SeqCst), 2);
    }
}
