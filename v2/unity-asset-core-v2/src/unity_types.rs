//! Unity types for async processing
//!
//! Async-friendly Unity data types and class registry system.

use crate::error::{Result, UnityAssetError};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// Trait for dynamic property access (copied from V1)
pub trait DynamicAccess {
    /// Get a property value with automatic type conversion
    fn get_dynamic(&self, key: &str) -> Option<DynamicValue>;

    /// Set a property value with automatic type conversion
    fn set_dynamic(&mut self, key: &str, value: DynamicValue) -> Result<()>;

    /// Check if a property exists
    fn has_dynamic(&self, key: &str) -> bool;

    /// Get all property names
    fn keys_dynamic(&self) -> Vec<String>;
}

/// Dynamic value wrapper that supports Python-like operations (copied from V1)
#[derive(Debug, Clone, PartialEq)]
pub enum DynamicValue {
    /// String value
    String(String),
    /// Integer value
    Integer(i64),
    /// Float value
    Float(f64),
    /// Boolean value
    Bool(bool),
    /// Array value
    Array(Vec<DynamicValue>),
    /// Object value
    Object(HashMap<String, DynamicValue>),
    /// Null value
    Null,
}

impl DynamicValue {
    /// Convert from UnityValue (copied from V1)
    pub fn from_unity_value(value: &UnityValue) -> Self {
        match value {
            UnityValue::String(s) => DynamicValue::String(s.clone()),
            UnityValue::Int(i) => DynamicValue::Integer(*i),
            UnityValue::Int32(i) => DynamicValue::Integer(*i as i64),
            UnityValue::UInt32(u) => DynamicValue::Integer(*u as i64),
            UnityValue::Int64(i) => DynamicValue::Integer(*i),
            UnityValue::UInt64(u) => DynamicValue::Integer(*u as i64),
            UnityValue::Float(f) => DynamicValue::Float(*f),
            UnityValue::Double(d) => DynamicValue::Float(*d),
            UnityValue::Bool(b) => DynamicValue::Bool(*b),
            UnityValue::Array(arr) => {
                let converted: Vec<DynamicValue> =
                    arr.iter().map(DynamicValue::from_unity_value).collect();
                DynamicValue::Array(converted)
            }
            UnityValue::Object(obj) => {
                let converted: HashMap<String, DynamicValue> = obj
                    .iter()
                    .map(|(k, v)| (k.clone(), DynamicValue::from_unity_value(v)))
                    .collect();
                DynamicValue::Object(converted)
            }
            UnityValue::Null => DynamicValue::Null,
            UnityValue::Bytes(_) => DynamicValue::Null, // Convert bytes to null for simplicity
        }
    }

    /// Convert to UnityValue (adapted from V1)
    pub fn to_unity_value(&self) -> UnityValue {
        match self {
            DynamicValue::String(s) => UnityValue::String(s.clone()),
            DynamicValue::Integer(i) => UnityValue::Int(*i),
            DynamicValue::Float(f) => UnityValue::Float(*f),
            DynamicValue::Bool(b) => UnityValue::Bool(*b),
            DynamicValue::Array(arr) => {
                let converted: Vec<UnityValue> =
                    arr.iter().map(DynamicValue::to_unity_value).collect();
                UnityValue::Array(converted)
            }
            DynamicValue::Object(obj) => {
                let converted: IndexMap<String, UnityValue> = obj
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_unity_value()))
                    .collect();
                UnityValue::Object(converted)
            }
            DynamicValue::Null => UnityValue::Null,
        }
    }

    /// Get as string (copied from V1)
    pub fn as_string(&self) -> Option<&str> {
        match self {
            DynamicValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Get as integer (copied from V1)
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            DynamicValue::Integer(i) => Some(*i),
            DynamicValue::Float(f) => Some(*f as i64),
            DynamicValue::Bool(b) => Some(if *b { 1 } else { 0 }),
            _ => None,
        }
    }

    /// Check if value is null (copied from V1)
    pub fn is_null(&self) -> bool {
        matches!(self, DynamicValue::Null)
    }
}

/// Placeholder AsyncTypeTree for async processing compatibility
#[derive(Debug, Clone)]
pub struct AsyncTypeTree {
    // Placeholder fields - in a full implementation this would contain
    // the actual type tree structure for parsing Unity objects
    pub class_id: i32,
    pub class_name: String,
}

impl AsyncTypeTree {
    pub fn new() -> Self {
        Self {
            class_id: 0,
            class_name: String::new(),
        }
    }
}

/// Async Unity class representation - based on UnityPy Object structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncUnityClass {
    /// Unity class ID
    pub class_id: i32,
    /// Class name
    pub class_name: String,
    /// YAML anchor for this object (for YAML format compatibility)
    pub anchor: String,
    /// Extra data after the anchor line (for Unity YAML format)
    pub extra_anchor_data: String,
    /// Path ID in the file (for binary format)
    pub path_id: Option<i64>,
    /// File ID (for compatibility)
    pub file_id: String,
    /// Object data (for compatibility)
    pub data: UnityValue,
    /// Object properties (similar to V1 structure)
    properties: IndexMap<String, UnityValue>,
    /// Object metadata
    pub metadata: ObjectMetadata,
}

impl AsyncUnityClass {
    /// Create new Unity class (YAML format)
    pub fn new(class_id: i32, class_name: String, anchor: String) -> Self {
        Self {
            class_id,
            class_name,
            anchor,
            extra_anchor_data: String::new(),
            path_id: None,
            file_id: "Unknown".to_string(),
            data: UnityValue::Null,
            properties: IndexMap::new(),
            metadata: ObjectMetadata::default(),
        }
    }

    /// Create new Unity class with path ID (binary format)
    pub fn with_path_id(class_id: i32, class_name: String, anchor: String, path_id: i64) -> Self {
        Self {
            class_id,
            class_name,
            anchor,
            extra_anchor_data: String::new(),
            path_id: Some(path_id),
            file_id: "Unknown".to_string(),
            data: UnityValue::Null,
            properties: IndexMap::new(),
            metadata: ObjectMetadata::default(),
        }
    }

    /// Get object name if available (similar to V1 implementation)
    pub fn name(&self) -> Option<String> {
        self.get("m_Name").and_then(|v| v.as_string())
    }

    /// Get class name
    pub fn class_name(&self) -> &str {
        &self.class_name
    }

    /// Get a property value (similar to V1 UnityClass)
    pub fn get(&self, key: &str) -> Option<&UnityValue> {
        self.properties.get(key)
    }

    /// Get a mutable property value
    pub fn get_mut(&mut self, key: &str) -> Option<&mut UnityValue> {
        self.properties.get_mut(key)
    }

    /// Set a property value
    pub fn set<V: Into<UnityValue>>(&mut self, key: String, value: V) {
        self.properties.insert(key, value.into());
    }

    /// Check if a property exists
    pub fn has_property(&self, key: &str) -> bool {
        self.properties.contains_key(key)
    }

    /// Get a property value by key
    pub fn get_property(&self, key: &str) -> Option<&UnityValue> {
        self.properties.get(key)
    }

    /// Get all property names
    pub fn property_names(&self) -> impl Iterator<Item = &String> {
        self.properties.keys()
    }

    /// Get all properties
    pub fn properties(&self) -> &IndexMap<String, UnityValue> {
        &self.properties
    }

    /// Get mutable properties
    pub fn properties_mut(&mut self) -> &mut IndexMap<String, UnityValue> {
        &mut self.properties
    }

    /// Get type tree if available (async version)
    pub async fn get_type_tree(&self) -> Option<AsyncTypeTree> {
        // TODO: Implement async type tree retrieval
        // For now, return None as placeholder
        None
    }

    /// Parse object with type tree (async version)
    pub async fn parse_with_typetree(
        &self,
        _type_tree: &AsyncTypeTree,
    ) -> Result<HashMap<String, UnityValue>> {
        // TODO: Implement async type tree parsing
        // For now, return empty map as placeholder
        Ok(HashMap::new())
    }

    /// Get raw binary data (async version)
    pub async fn get_raw_data(&self) -> Result<Vec<u8>> {
        // Try to extract raw data from properties
        if let Some(UnityValue::Bytes(bytes)) = self.get("_raw_data") {
            Ok(bytes.clone())
        } else {
            // Try to serialize all properties as bytes
            let yaml_content = serde_yaml::to_string(&self.properties)
                .map_err(|e| UnityAssetError::Serialization(e.to_string()))?;
            Ok(yaml_content.into_bytes())
        }
    }

    /// Check if class matches any of the given names
    pub fn matches_class(&self, class_names: &[&str]) -> bool {
        class_names.iter().any(|&name| self.class_name == name)
    }

    /// Get data as specific type
    pub fn get_data<T>(&self) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let yaml_content = serde_yaml::to_string(&self.properties)
            .map_err(|e| UnityAssetError::Serialization(e.to_string()))?;
        serde_yaml::from_str(&yaml_content)
            .map_err(|e| UnityAssetError::Serialization(e.to_string()))
    }

    /// Convert to legacy Unity class (for compatibility)
    pub fn to_legacy(&self) -> LegacyUnityClass {
        LegacyUnityClass {
            class_id: self.class_id,
            class_name: self.class_name.clone(),
            anchor: self.anchor.clone(),
            properties: self.properties.clone(),
        }
    }
}

/// Implementation of dynamic property access for AsyncUnityClass (copied from V1)
impl DynamicAccess for AsyncUnityClass {
    fn get_dynamic(&self, key: &str) -> Option<DynamicValue> {
        self.properties.get(key).map(DynamicValue::from_unity_value)
    }

    fn set_dynamic(&mut self, key: &str, value: DynamicValue) -> Result<()> {
        self.properties
            .insert(key.to_string(), value.to_unity_value());
        Ok(())
    }

    fn has_dynamic(&self, key: &str) -> bool {
        self.properties.contains_key(key)
    }

    fn keys_dynamic(&self) -> Vec<String> {
        self.properties.keys().cloned().collect()
    }
}

/// Async Unity value system
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UnityValue {
    Null,
    Bool(bool),
    Int(i64),    // Generic integer
    Int32(i32),  // Specific 32-bit signed
    UInt32(u32), // Specific 32-bit unsigned
    Int64(i64),  // Specific 64-bit signed
    UInt64(u64), // Specific 64-bit unsigned
    Float(f64),  // Generic float
    Double(f64), // Specific double
    String(String),
    Array(Vec<UnityValue>),
    Object(IndexMap<String, UnityValue>),
    Bytes(Vec<u8>), // Binary data
}

impl UnityValue {
    /// Get value as string
    pub fn get_string(&self, key: &str) -> Option<String> {
        match self {
            Self::Object(map) => map.get(key)?.as_string(),
            _ => None,
        }
    }

    /// Get value as integer
    pub fn get_int(&self, key: &str) -> Option<i64> {
        match self {
            Self::Object(map) => map.get(key)?.as_int(),
            _ => None,
        }
    }

    /// Get value as float
    pub fn get_float(&self, key: &str) -> Option<f64> {
        match self {
            Self::Object(map) => map.get(key)?.as_float(),
            _ => None,
        }
    }

    /// Get value as bool
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self {
            Self::Object(map) => map.get(key)?.as_bool(),
            _ => None,
        }
    }

    /// Get value as array
    pub fn get_array(&self, key: &str) -> Option<&Vec<UnityValue>> {
        match self {
            Self::Object(map) => map.get(key)?.as_array(),
            _ => None,
        }
    }

    /// Convert to string if possible
    pub fn as_string(&self) -> Option<String> {
        match self {
            Self::String(s) => Some(s.clone()),
            Self::Int(i) => Some(i.to_string()),
            Self::Float(f) => Some(f.to_string()),
            Self::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// Get as string reference (similar to V1 UnityValue)
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Convert to integer if possible
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            Self::Int32(i) => Some(*i as i64),
            Self::Int64(i) => Some(*i),
            Self::UInt32(u) => Some(*u as i64),
            Self::UInt64(u) => Some(*u as i64),
            Self::Float(f) => Some(*f as i64),
            Self::Double(d) => Some(*d as i64),
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Convert to float if possible
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(f) => Some(*f),
            Self::Double(d) => Some(*d),
            Self::Int(i) => Some(*i as f64),
            Self::Int32(i) => Some(*i as f64),
            Self::Int64(i) => Some(*i as f64),
            Self::UInt32(u) => Some(*u as f64),
            Self::UInt64(u) => Some(*u as f64),
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Convert to bool if possible
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            Self::Int(i) => Some(*i != 0),
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Convert to array if possible
    pub fn as_array(&self) -> Option<&Vec<UnityValue>> {
        match self {
            Self::Array(arr) => Some(arr),
            _ => None,
        }
    }

    /// Convert to object if possible
    pub fn as_object(&self) -> Option<&IndexMap<String, UnityValue>> {
        match self {
            Self::Object(obj) => Some(obj),
            _ => None,
        }
    }

    /// Convert to bytes if possible
    pub fn as_bytes(&self) -> Option<&Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Convert to i32 if possible
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Self::Int32(i) => Some(*i),
            Self::Int(i) => Some(*i as i32),
            Self::Int64(i) => Some(*i as i32),
            Self::UInt32(u) => Some(*u as i32),
            Self::UInt64(u) => Some(*u as i32),
            Self::Float(f) => Some(*f as i32),
            Self::Double(d) => Some(*d as i32),
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Convert to u32 if possible
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Self::UInt32(u) => Some(*u),
            Self::Int32(i) => {
                if *i >= 0 {
                    Some(*i as u32)
                } else {
                    None
                }
            }
            Self::Int(i) => {
                if *i >= 0 {
                    Some(*i as u32)
                } else {
                    None
                }
            }
            Self::Int64(i) => {
                if *i >= 0 {
                    Some(*i as u32)
                } else {
                    None
                }
            }
            Self::UInt64(u) => Some(*u as u32),
            Self::Float(f) => {
                if *f >= 0.0 {
                    Some(*f as u32)
                } else {
                    None
                }
            }
            Self::Double(d) => {
                if *d >= 0.0 {
                    Some(*d as u32)
                } else {
                    None
                }
            }
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Convert to i64 if possible
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            Self::Float(f) => Some(*f as i64),
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Convert to u8 if possible
    pub fn as_u8(&self) -> Option<u8> {
        match self {
            Self::Int(i) => {
                if *i >= 0 && *i <= u8::MAX as i64 {
                    Some(*i as u8)
                } else {
                    None
                }
            }
            Self::Float(f) => {
                if *f >= 0.0 && *f <= u8::MAX as f64 {
                    Some(*f as u8)
                } else {
                    None
                }
            }
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Convert to u64 if possible
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::UInt64(u) => Some(*u),
            Self::UInt32(u) => Some(*u as u64),
            Self::Int64(i) => {
                if *i >= 0 {
                    Some(*i as u64)
                } else {
                    None
                }
            }
            Self::Int32(i) => {
                if *i >= 0 {
                    Some(*i as u64)
                } else {
                    None
                }
            }
            Self::Int(i) => {
                if *i >= 0 {
                    Some(*i as u64)
                } else {
                    None
                }
            }
            Self::Float(f) => {
                if *f >= 0.0 {
                    Some(*f as u64)
                } else {
                    None
                }
            }
            Self::Double(d) => {
                if *d >= 0.0 {
                    Some(*d as u64)
                } else {
                    None
                }
            }
            Self::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Check if value is null/empty
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Get size in bytes (approximate)
    pub fn size_bytes(&self) -> usize {
        match self {
            Self::Null => 0,
            Self::Bool(_) => 1,
            Self::Int(_) => 8,
            Self::Int32(_) => 4,
            Self::UInt32(_) => 4,
            Self::Int64(_) => 8,
            Self::UInt64(_) => 8,
            Self::Float(_) => 8,
            Self::Double(_) => 8,
            Self::String(s) => s.len(),
            Self::Array(arr) => arr.iter().map(|v| v.size_bytes()).sum::<usize>() + 24,
            Self::Object(obj) => {
                obj.iter()
                    .map(|(k, v)| k.len() + v.size_bytes())
                    .sum::<usize>()
                    + 24
            }
            Self::Bytes(bytes) => bytes.len(),
        }
    }

    /// Serialize to string
    pub fn serialize(&self) -> Result<String> {
        serde_yaml::to_string(self).map_err(|e| UnityAssetError::Serialization(e.to_string()))
    }

    /// Deserialize from value
    pub fn deserialize<T>(&self) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        // Convert to YAML and deserialize
        let yaml_string = serde_yaml::to_string(self)
            .map_err(|e| UnityAssetError::Serialization(e.to_string()))?;
        serde_yaml::from_str(&yaml_string)
            .map_err(|e| UnityAssetError::Serialization(e.to_string()))
    }

    /// Try to create UnityValue from serde-compatible value
    pub fn try_from_serde<T>(value: &T) -> Result<Self>
    where
        T: Serialize,
    {
        let yaml_value = serde_yaml::to_value(value)
            .map_err(|e| UnityAssetError::Serialization(e.to_string()))?;
        Self::from_serde_value(yaml_value)
    }

    /// Convert from serde_yaml::Value
    pub fn from_serde_value(value: serde_yaml::Value) -> Result<Self> {
        let result = match value {
            serde_yaml::Value::Null => UnityValue::Null,
            serde_yaml::Value::Bool(b) => UnityValue::Bool(b),
            serde_yaml::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    UnityValue::Int(i)
                } else if let Some(f) = n.as_f64() {
                    UnityValue::Float(f)
                } else {
                    UnityValue::String(n.to_string())
                }
            }
            serde_yaml::Value::String(s) => UnityValue::String(s),
            serde_yaml::Value::Sequence(seq) => {
                let mut arr = Vec::new();
                for item in seq {
                    arr.push(Self::from_serde_value(item)?);
                }
                UnityValue::Array(arr)
            }
            serde_yaml::Value::Mapping(map) => {
                let mut obj = IndexMap::new();
                for (k, v) in map {
                    let key = match k {
                        serde_yaml::Value::String(s) => s,
                        _ => format!("{:?}", k),
                    };
                    obj.insert(key, Self::from_serde_value(v)?);
                }
                UnityValue::Object(obj)
            }
            serde_yaml::Value::Tagged(tagged) => Self::from_serde_value(tagged.value)?,
        };
        Ok(result)
    }
}

impl From<UnityValue> for serde_yaml::Value {
    fn from(value: UnityValue) -> Self {
        match value {
            UnityValue::Null => serde_yaml::Value::Null,
            UnityValue::Bool(b) => serde_yaml::Value::Bool(b),
            UnityValue::Int(i) => serde_yaml::Value::Number(i.into()),
            UnityValue::Int32(i) => serde_yaml::Value::Number(i.into()),
            UnityValue::UInt32(u) => serde_yaml::Value::Number(u.into()),
            UnityValue::Int64(i) => serde_yaml::Value::Number(i.into()),
            UnityValue::UInt64(u) => serde_yaml::Value::Number(u.into()),
            UnityValue::Float(f) => serde_yaml::Value::Number(f.into()),
            UnityValue::Double(d) => serde_yaml::Value::Number(d.into()),
            UnityValue::String(s) => serde_yaml::Value::String(s),
            UnityValue::Array(arr) => {
                serde_yaml::Value::Sequence(arr.into_iter().map(Into::into).collect())
            }
            UnityValue::Object(obj) => {
                let mapping: serde_yaml::Mapping = obj
                    .into_iter()
                    .map(|(k, v)| (serde_yaml::Value::String(k), v.into()))
                    .collect();
                serde_yaml::Value::Mapping(mapping)
            }
            UnityValue::Bytes(_) => serde_yaml::Value::Null, // Cannot represent binary in YAML
        }
    }
}

/// Dynamic value wrapper for runtime access
#[derive(Debug, Clone)]
pub struct AsyncDynamicValue {
    value: UnityValue,
    path: Vec<String>,
}

impl AsyncDynamicValue {
    /// Create new dynamic value
    pub fn new(value: UnityValue) -> Self {
        Self {
            value,
            path: Vec::new(),
        }
    }

    /// Navigate to nested property
    pub fn get(&self, key: &str) -> Option<AsyncDynamicValue> {
        match &self.value {
            UnityValue::Object(obj) => obj.get(key).map(|v| {
                let mut path = self.path.clone();
                path.push(key.to_string());
                AsyncDynamicValue {
                    value: v.clone(),
                    path,
                }
            }),
            _ => None,
        }
    }

    /// Get array element
    pub fn get_index(&self, index: usize) -> Option<AsyncDynamicValue> {
        match &self.value {
            UnityValue::Array(arr) => arr.get(index).map(|v| {
                let mut path = self.path.clone();
                path.push(format!("[{}]", index));
                AsyncDynamicValue {
                    value: v.clone(),
                    path,
                }
            }),
            _ => None,
        }
    }

    /// Get current path as string
    pub fn path(&self) -> String {
        self.path.join(".")
    }

    /// Get underlying value
    pub fn value(&self) -> &UnityValue {
        &self.value
    }
}

/// Object metadata
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectMetadata {
    /// File path where object was loaded from
    pub file_path: Option<String>,
    /// Size in bytes
    pub size_bytes: u64,
    /// Creation timestamp
    pub created_at: Option<std::time::SystemTime>,
    /// Last modified timestamp
    pub modified_at: Option<std::time::SystemTime>,
    /// Custom properties
    pub properties: HashMap<String, String>,
}

/// Unity class registry for mapping class IDs to names
pub struct UnityClassRegistry {
    id_to_name: RwLock<HashMap<i32, String>>,
    name_to_id: RwLock<HashMap<String, i32>>,
}

impl UnityClassRegistry {
    /// Create new registry
    pub fn new() -> Self {
        Self {
            id_to_name: RwLock::new(HashMap::new()),
            name_to_id: RwLock::new(HashMap::new()),
        }
    }

    /// Register class ID and name mapping
    pub fn register(&self, class_id: i32, class_name: String) -> Result<()> {
        let mut id_to_name = self.id_to_name.write().map_err(|_| {
            UnityAssetError::Concurrency("Failed to acquire write lock".to_string())
        })?;
        let mut name_to_id = self.name_to_id.write().map_err(|_| {
            UnityAssetError::Concurrency("Failed to acquire write lock".to_string())
        })?;

        id_to_name.insert(class_id, class_name.clone());
        name_to_id.insert(class_name, class_id);

        Ok(())
    }

    /// Get class name from ID
    pub fn get_class_name(&self, class_id: i32) -> Option<String> {
        self.id_to_name.read().ok()?.get(&class_id).cloned()
    }

    /// Get class ID from name
    pub fn get_class_id(&self, class_name: &str) -> Option<i32> {
        self.name_to_id.read().ok()?.get(class_name).copied()
    }

    /// Load default Unity class mappings
    pub fn load_defaults(&self) -> Result<()> {
        // Common Unity class IDs
        let defaults = vec![
            (1, "GameObject"),
            (2, "Component"),
            (4, "Transform"),
            (20, "Camera"),
            (21, "Material"),
            (23, "MeshRenderer"),
            (25, "Renderer"),
            (28, "Texture2D"),
            (43, "Mesh"),
            (48, "Shader"),
            (83, "AudioClip"),
            (212, "SpriteRenderer"),
            (213, "Sprite"),
            (224, "RectTransform"),
        ];

        for (id, name) in defaults {
            self.register(id, name.to_string())?;
        }

        Ok(())
    }
}

impl Default for UnityClassRegistry {
    fn default() -> Self {
        let registry = Self::new();
        registry.load_defaults().unwrap_or_else(|e| {
            eprintln!("Failed to load default class registry: {}", e);
        });
        registry
    }
}

/// Global class registry instance
static GLOBAL_REGISTRY: Lazy<UnityClassRegistry> = Lazy::new(UnityClassRegistry::default);

/// Get global class registry
pub fn global_class_registry() -> &'static UnityClassRegistry {
    &GLOBAL_REGISTRY
}

/// Legacy Unity class for compatibility
#[derive(Debug, Clone)]
pub struct LegacyUnityClass {
    pub class_id: i32,
    pub class_name: String,
    pub anchor: String,
    pub properties: IndexMap<String, UnityValue>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_async_unity_value() {
        let mut obj = IndexMap::new();
        obj.insert(
            "name".to_string(),
            UnityValue::String("TestObject".to_string()),
        );
        obj.insert("active".to_string(), UnityValue::Bool(true));
        obj.insert("count".to_string(), UnityValue::Int(42));

        let value = UnityValue::Object(obj);

        assert_eq!(value.get_string("name"), Some("TestObject".to_string()));
        assert_eq!(value.get_bool("active"), Some(true));
        assert_eq!(value.get_int("count"), Some(42));
    }

    #[test]
    fn test_dynamic_value() {
        let mut obj = IndexMap::new();
        obj.insert(
            "transform".to_string(),
            UnityValue::Object({
                let mut transform = IndexMap::new();
                transform.insert(
                    "position".to_string(),
                    UnityValue::Array(vec![
                        UnityValue::Float(1.0),
                        UnityValue::Float(2.0),
                        UnityValue::Float(3.0),
                    ]),
                );
                transform
            }),
        );

        let dynamic = AsyncDynamicValue::new(UnityValue::Object(obj));
        let transform = dynamic.get("transform").unwrap();
        let position = transform.get("position").unwrap();
        let x = position.get_index(0).unwrap();

        assert_eq!(x.value().as_float(), Some(1.0));
        assert_eq!(x.path(), "transform.position.[0]");
    }

    #[tokio::test]
    async fn test_class_registry() {
        let registry = UnityClassRegistry::new();
        registry.register(999, "CustomClass".to_string()).unwrap();

        assert_eq!(
            registry.get_class_name(999),
            Some("CustomClass".to_string())
        );
        assert_eq!(registry.get_class_id("CustomClass"), Some(999));
    }

    #[test]
    fn test_unity_class_creation() {
        let data = UnityValue::Object({
            let mut obj = IndexMap::new();
            obj.insert(
                "m_Name".to_string(),
                UnityValue::String("Player".to_string()),
            );
            obj
        });

        let mut class = AsyncUnityClass::new(1, "GameObject".to_string(), "0".to_string());
        *class.properties_mut() = data.as_object().unwrap().clone();

        assert_eq!(class.name(), Some("Player".to_string()));
        assert_eq!(class.class_name(), "GameObject");
        assert!(class.matches_class(&["GameObject", "Transform"]));
    }
}
