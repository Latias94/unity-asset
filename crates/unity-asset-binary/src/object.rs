//! Unity object representation and helpers.

use crate::asset::{ObjectInfo, SerializedFile};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
use crate::typetree::{
    PPtrScanResult, TypeTree, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeParseOutput,
    TypeTreeParseWarning, TypeTreeSerializer,
};
use crate::unity_objects::{GameObject, Transform};
use std::sync::Arc;
use unity_asset_core::{UnityClass, UnityValue};

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
        self.info.path_id
    }

    pub fn class_id(&self) -> i32 {
        self.info.type_id
    }

    pub fn byte_start(&self) -> u64 {
        self.info.byte_start
    }

    pub fn byte_size(&self) -> u32 {
        self.info.byte_size
    }

    /// Get the raw bytes for this object (preloaded if available, otherwise sliced from the file).
    pub fn raw_data(&self) -> Result<&'a [u8]> {
        if !self.info.data.is_empty() {
            return Ok(self.info.data.as_slice());
        }
        self.file.object_bytes(self.info)
    }

    /// Parse this object into an owned [`UnityObject`] (best-effort).
    pub fn read(&self) -> Result<UnityObject> {
        UnityObject::from_serialized_file(self.file, self.info)
    }

    pub fn read_with_options(&self, options: TypeTreeParseOptions) -> Result<UnityObject> {
        UnityObject::from_serialized_file_with_options(self.file, self.info, options)
    }

    /// Peek the object's name (`m_Name`/`name`) without parsing the full TypeTree.
    ///
    /// This mirrors UnityPy's `ObjectReader.peek_name()` behavior by parsing only a prefix of the
    /// root TypeTree until the name field, when possible.
    pub fn peek_name(&self) -> Result<Option<String>> {
        self.peek_name_with_options(TypeTreeParseOptions {
            mode: TypeTreeParseMode::Lenient,
        })
    }

    pub fn peek_name_with_options(&self, options: TypeTreeParseOptions) -> Result<Option<String>> {
        let Some(tree) = type_tree_for_object(self.file, self.info) else {
            return Ok(None);
        };
        let tree = tree.as_ref();
        let Some((prefix_len, field)) = tree.name_peek_prefix() else {
            return Ok(None);
        };

        let bytes = self.raw_data()?;
        let mut reader = BinaryReader::new(bytes, self.file.header.byte_order());
        let serializer = TypeTreeSerializer::new(tree);
        let out = serializer.parse_object_prefix_detailed(&mut reader, options, prefix_len)?;

        match out.properties.get(&field) {
            Some(UnityValue::String(s)) => Ok(Some(s.clone())),
            _ => Ok(None),
        }
    }

    /// Scan TypeTree-based object bytes and collect `PPtr` references (`fileID`, `pathID`) without
    /// allocating a full parsed `UnityValue` tree.
    pub fn scan_pptrs(&self) -> Result<Option<PPtrScanResult>> {
        let Some(tree) = type_tree_for_object(self.file, self.info) else {
            return Ok(None);
        };
        let tree = tree.as_ref();
        if tree.is_empty() {
            return Ok(None);
        }

        let bytes = self.raw_data()?;
        let mut reader = BinaryReader::new(bytes, self.file.header.byte_order());
        let serializer = TypeTreeSerializer::new(tree);
        if self.file.ref_types.is_empty() {
            Ok(Some(serializer.scan_pptrs(&mut reader)?))
        } else {
            Ok(Some(serializer.scan_pptrs_with_ref_types(
                &mut reader,
                Some(&self.file.ref_types),
            )?))
        }
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

const RAW_DATA_INLINE_LIMIT: usize = 4 * 1024;
const RAW_DATA_PREVIEW_LEN: usize = 256;

impl ObjectBytes {
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
        }
    }

    /// Create a UnityObject from raw bytes without TypeTree information.
    ///
    /// For large objects, this intentionally avoids expanding all bytes into a `UnityValue::Array`
    /// to reduce memory pressure and parsing time; use `raw_data()` instead.
    pub fn from_raw(class_id: i32, path_id: i64, data: Vec<u8>) -> Self {
        let info = ObjectInfo::new(path_id, 0, data.len() as u32, class_id, -1);
        let raw = ObjectBytes::Inline(data);
        let mut class =
            UnityClass::new(class_id, class_name_from_id(class_id), path_id.to_string());
        let bytes = raw.as_slice();
        class.set(
            "_raw_data_len".to_string(),
            UnityValue::Integer(bytes.len() as i64),
        );
        if bytes.len() <= RAW_DATA_INLINE_LIMIT {
            class.set(
                "_raw_data".to_string(),
                UnityValue::Array(
                    bytes
                        .iter()
                        .copied()
                        .map(|b| UnityValue::Integer(b as i64))
                        .collect(),
                ),
            );
        } else {
            class.set("_raw_data_truncated".to_string(), UnityValue::Bool(true));
            let preview = bytes
                .iter()
                .take(RAW_DATA_PREVIEW_LEN)
                .copied()
                .map(|b| UnityValue::Integer(b as i64))
                .collect();
            class.set("_raw_data_preview".to_string(), UnityValue::Array(preview));
        }
        Self {
            info,
            class,
            byte_order: ByteOrder::Little,
            raw,
            typetree_warnings: Vec::new(),
        }
    }

    /// Create a UnityObject from a SerializedFile + ObjectInfo, using TypeTree when available.
    pub fn from_serialized_file(file: &SerializedFile, info: &ObjectInfo) -> Result<Self> {
        Self::from_serialized_file_with_options(file, info, TypeTreeParseOptions::default())
    }

    pub fn from_serialized_file_with_options(
        file: &SerializedFile,
        info: &ObjectInfo,
        options: TypeTreeParseOptions,
    ) -> Result<Self> {
        let class_id = info.type_id;
        let type_tree = type_tree_for_object(file, info);
        let byte_order = file.header.byte_order();
        let (start, end) = object_range(file, info)?;
        let base = file.data_base_offset();
        let raw = ObjectBytes::Shared {
            data: file.data_shared(),
            start: base + start,
            end: base + end,
        };

        let mut class = UnityClass::new(
            class_id,
            class_name_from_id(class_id),
            info.path_id.to_string(),
        );

        let mut warnings: Vec<TypeTreeParseWarning> = Vec::new();

        if let Some(tree) = type_tree {
            let tree = tree.as_ref();
            match parse_object_data(file, info, byte_order, tree, options) {
                Ok(out) => {
                    class.update_properties(out.properties);
                    warnings = out.warnings;
                }
                Err(e) => match options.mode {
                    TypeTreeParseMode::Strict => return Err(e),
                    TypeTreeParseMode::Lenient => {
                        warnings.push(TypeTreeParseWarning {
                            field: "<root>".to_string(),
                            error: e.to_string(),
                        });
                        apply_raw_preview(&mut class, raw.as_slice());
                    }
                },
            }
        } else {
            apply_raw_preview(&mut class, raw.as_slice());
        }

        Ok(Self {
            info: {
                let mut cloned = info.clone();
                cloned.data.clear();
                cloned
            },
            class,
            byte_order,
            raw,
            typetree_warnings: warnings,
        })
    }

    pub fn path_id(&self) -> i64 {
        self.info.path_id
    }

    pub fn class_id(&self) -> i32 {
        self.info.type_id
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

    pub fn byte_size(&self) -> u32 {
        self.info.byte_size
    }

    pub fn byte_start(&self) -> u64 {
        self.info.byte_start
    }

    pub fn byte_order(&self) -> ByteOrder {
        self.byte_order
    }
}

fn class_name_from_id(class_id: i32) -> String {
    unity_asset_core::get_class_name(class_id).unwrap_or_else(|| format!("Class_{}", class_id))
}

enum TypeTreeSource<'a> {
    Borrowed(&'a TypeTree),
    Shared(Arc<TypeTree>),
}

impl TypeTreeSource<'_> {
    fn as_ref(&self) -> &TypeTree {
        match self {
            Self::Borrowed(t) => t,
            Self::Shared(t) => t.as_ref(),
        }
    }
}

fn type_tree_for_object<'a>(
    file: &'a SerializedFile,
    info: &ObjectInfo,
) -> Option<TypeTreeSource<'a>> {
    fn from_internal<'a>(file: &'a SerializedFile, info: &ObjectInfo) -> Option<&'a TypeTree> {
        if info.type_index >= 0 {
            return file
                .types
                .get(info.type_index as usize)
                .map(|t| &t.type_tree);
        }
        file.types
            .iter()
            .find(|t| t.class_id == info.type_id)
            .map(|t| &t.type_tree)
    }

    if file.enable_type_tree
        && let Some(tree) = from_internal(file, info)
        && !tree.is_empty()
    {
        return Some(TypeTreeSource::Borrowed(tree));
    }

    // Best-effort fallback: stripped files can supply a registry externally.
    // We also allow this fallback even when `enable_type_tree = true` but the internal entry is missing/empty.
    file.type_tree_registry
        .as_ref()
        .and_then(|r| r.resolve(&file.unity_version, info.type_id))
        .map(TypeTreeSource::Shared)
}

fn object_bytes<'a>(file: &'a SerializedFile, info: &'a ObjectInfo) -> Result<&'a [u8]> {
    if !info.data.is_empty() {
        return Ok(&info.data);
    }
    file.object_bytes(info)
}

fn object_range(file: &SerializedFile, info: &ObjectInfo) -> Result<(usize, usize)> {
    let start: usize = info.byte_start.try_into().map_err(|_| {
        BinaryError::invalid_data(format!("Object byte_start overflow: {}", info.byte_start))
    })?;
    let end = start.saturating_add(info.byte_size as usize);
    if end > file.data().len() {
        return Err(BinaryError::invalid_data(format!(
            "Object data out of bounds (path_id={}, start={}, size={}, file_len={})",
            info.path_id,
            start,
            info.byte_size,
            file.data().len()
        )));
    }
    Ok((start, end))
}

fn parse_object_data(
    file: &SerializedFile,
    info: &ObjectInfo,
    byte_order: ByteOrder,
    tree: &TypeTree,
    options: TypeTreeParseOptions,
) -> Result<TypeTreeParseOutput> {
    let bytes = object_bytes(file, info)?;
    let mut reader = BinaryReader::new(bytes, byte_order);
    let serializer = TypeTreeSerializer::new(tree);
    if file.ref_types.is_empty() {
        serializer.parse_object_detailed(&mut reader, options)
    } else {
        serializer.parse_object_detailed_with_ref_types(&mut reader, options, &file.ref_types)
    }
}

fn apply_raw_preview(class: &mut UnityClass, bytes: &[u8]) {
    class.set(
        "_raw_data_len".to_string(),
        UnityValue::Integer(bytes.len() as i64),
    );
    if bytes.len() <= RAW_DATA_INLINE_LIMIT {
        class.set("_raw_data".to_string(), UnityValue::Bytes(bytes.to_vec()));
    } else {
        class.set("_raw_data_truncated".to_string(), UnityValue::Bool(true));
        let preview_len = bytes.len().min(RAW_DATA_PREVIEW_LEN);
        class.set(
            "_raw_data_preview".to_string(),
            UnityValue::Bytes(bytes[..preview_len].to_vec()),
        );
    }
}
