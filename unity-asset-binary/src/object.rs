//! Unity object representation and helpers.

use crate::asset::{ObjectInfo, SerializedFile};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::typetree::{TypeTree, TypeTreeSerializer};
use crate::unity_objects::{GameObject, Transform};
use std::sync::Arc;
use unity_asset_core::{UnityClass, UnityValue};

#[derive(Debug, Clone)]
enum ObjectBytes {
    Empty,
    Inline(Vec<u8>),
    Shared {
        data: Arc<[u8]>,
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
            ObjectBytes::Shared { data, start, end } => &data[*start..*end],
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
}

impl UnityObject {
    /// Create a UnityObject from an already-parsed UnityClass (used by tests and higher-level code).
    pub fn from_info_and_class(info: ObjectInfo, class: UnityClass) -> Self {
        Self {
            byte_order: ByteOrder::Little,
            info,
            class,
            raw: ObjectBytes::Empty,
        }
    }

    /// Create a UnityObject from raw bytes without TypeTree information.
    ///
    /// For large objects, this intentionally avoids expanding all bytes into a `UnityValue::Array`
    /// to reduce memory pressure and parsing time; use `raw_data()` instead.
    pub fn from_raw(class_id: i32, path_id: i64, data: Vec<u8>) -> Self {
        let info = ObjectInfo::new(path_id, 0, data.len() as u32, class_id, -1);
        let raw = ObjectBytes::Inline(data);
        let mut class = UnityClass::new(class_id, class_name_from_id(class_id), path_id.to_string());
        let bytes = raw.as_slice();
        class.set("_raw_data_len".to_string(), UnityValue::Integer(bytes.len() as i64));
        if bytes.len() <= RAW_DATA_INLINE_LIMIT {
            class.set(
                "_raw_data".to_string(),
                UnityValue::Array(bytes.iter().copied().map(|b| UnityValue::Integer(b as i64)).collect()),
            );
        } else {
            class.set("_raw_data_truncated".to_string(), UnityValue::Bool(true));
            let preview = bytes.iter().take(RAW_DATA_PREVIEW_LEN).copied().map(|b| UnityValue::Integer(b as i64)).collect();
            class.set("_raw_data_preview".to_string(), UnityValue::Array(preview));
        }
        Self {
            info,
            class,
            byte_order: ByteOrder::Little,
            raw,
        }
    }

    /// Create a UnityObject from a SerializedFile + ObjectInfo, using TypeTree when available.
    pub fn from_serialized_file(file: &SerializedFile, info: &ObjectInfo) -> Result<Self> {
        let class_id = info.type_id;
        let type_tree = type_tree_for_object(file, info);
        let byte_order = file.header.byte_order();
        let (start, end) = object_range(file, info)?;
        let raw = ObjectBytes::Shared {
            data: file.data_arc(),
            start,
            end,
        };

        let mut class = UnityClass::new(class_id, class_name_from_id(class_id), info.path_id.to_string());

        if let Some(tree) = type_tree {
            match parse_object_data(file, info, byte_order, tree) {
                Ok(properties) => class.update_properties(properties),
                Err(_) => {
                    let bytes = raw.as_slice();
                    class.set("_raw_data_len".to_string(), UnityValue::Integer(bytes.len() as i64));
                    if bytes.len() <= RAW_DATA_INLINE_LIMIT {
                        class.set(
                            "_raw_data".to_string(),
                            UnityValue::Array(bytes.iter().copied().map(|b| UnityValue::Integer(b as i64)).collect()),
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
                }
            }
        } else {
            let bytes = raw.as_slice();
            class.set("_raw_data_len".to_string(), UnityValue::Integer(bytes.len() as i64));
            if bytes.len() <= RAW_DATA_INLINE_LIMIT {
                class.set(
                    "_raw_data".to_string(),
                    UnityValue::Array(bytes.iter().copied().map(|b| UnityValue::Integer(b as i64)).collect()),
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

fn type_tree_for_object<'a>(file: &'a SerializedFile, info: &ObjectInfo) -> Option<&'a TypeTree> {
    if !file.enable_type_tree {
        return None;
    }

    if info.type_index >= 0 {
        return file.types.get(info.type_index as usize).map(|t| &t.type_tree);
    }

    file.types.iter().find(|t| t.class_id == info.type_id).map(|t| &t.type_tree)
}

fn object_bytes<'a>(file: &'a SerializedFile, info: &'a ObjectInfo) -> Result<&'a [u8]> {
    if !info.data.is_empty() {
        return Ok(&info.data);
    }
    file.object_bytes(info)
}

fn object_range(file: &SerializedFile, info: &ObjectInfo) -> Result<(usize, usize)> {
    let start: usize = info
        .byte_start
        .try_into()
        .map_err(|_| BinaryError::invalid_data(format!("Object byte_start overflow: {}", info.byte_start)))?;
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
) -> Result<indexmap::IndexMap<String, UnityValue>> {
    let bytes = object_bytes(file, info)?;
    let mut reader = BinaryReader::new(bytes, byte_order);
    let serializer = TypeTreeSerializer::new(tree);
    serializer.parse_object(&mut reader)
}
