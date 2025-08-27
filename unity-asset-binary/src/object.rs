//! Unity object parsing and representation

use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::typetree::{TypeTree, TypeTreeNode};
use crate::unity_objects::{GameObject, Transform};
// Removed unused serde imports
use std::collections::HashMap;
use unity_asset_core::{UnityClass, UnityValue};

/// Information about a Unity object in a serialized file
#[derive(Debug, Clone)]
pub struct ObjectInfo {
    /// Path ID (unique identifier within the file)
    pub path_id: i64,
    /// Byte offset in the data section
    pub byte_start: u64,
    /// Size of the object data in bytes
    pub byte_size: u32,
    /// Class ID of the object
    pub class_id: i32,
    /// Type ID (used for type lookup)
    pub type_id: i32,
    /// Byte order for reading this object
    pub byte_order: ByteOrder,
    /// Raw object data
    pub data: Vec<u8>,
    /// Type information for this object
    pub type_tree: Option<TypeTree>,
}

impl ObjectInfo {
    /// Create a new ObjectInfo
    pub fn new(path_id: i64, byte_start: u64, byte_size: u32, class_id: i32) -> Self {
        Self {
            path_id,
            byte_start,
            byte_size,
            class_id,
            type_id: class_id, // Default to same as class_id
            byte_order: ByteOrder::Little,
            data: Vec::new(),
            type_tree: None,
        }
    }

    /// Get a binary reader for this object's data
    pub fn reader(&self) -> BinaryReader<'_> {
        BinaryReader::new(&self.data, self.byte_order)
    }

    /// Get the Unity class name for this object
    pub fn class_name(&self) -> String {
        unity_asset_core::get_class_name(self.class_id)
            .unwrap_or_else(|| format!("Class_{}", self.class_id))
    }

    /// Parse this object into a UnityClass using TypeTree information
    pub fn parse_object(&self) -> Result<UnityClass> {
        let mut unity_class =
            UnityClass::new(self.class_id, self.class_name(), self.path_id.to_string());

        if let Some(ref type_tree) = self.type_tree {
            let mut reader = self.reader();
            let properties = self.parse_with_typetree(&mut reader, type_tree)?;

            for (key, value) in properties {
                unity_class.set(key, value);
            }
        } else {
            // Fallback: try to parse as raw data
            unity_class.set(
                "_raw_data".to_string(),
                UnityValue::Array(
                    self.data
                        .iter()
                        .map(|&b| UnityValue::Integer(b as i64))
                        .collect(),
                ),
            );
        }

        Ok(unity_class)
    }

    /// Parse object data using TypeTree information
    fn parse_with_typetree(
        &self,
        reader: &mut BinaryReader,
        type_tree: &TypeTree,
    ) -> Result<HashMap<String, UnityValue>> {
        let mut properties = HashMap::new();

        if let Some(root) = type_tree.nodes.first() {
            self.parse_node(reader, root, &mut properties)?;
        }

        Ok(properties)
    }

    /// Parse a single TypeTree node
    fn parse_node(
        &self,
        reader: &mut BinaryReader,
        node: &TypeTreeNode,
        properties: &mut HashMap<String, UnityValue>,
    ) -> Result<()> {
        if node.name.is_empty() || !node.name.starts_with("m_") {
            // Skip nodes without proper names or that aren't member variables
            return Ok(());
        }

        let value = match node.type_name.as_str() {
            "bool" => UnityValue::Bool(reader.read_bool()?),
            "SInt8" => UnityValue::Integer(reader.read_i8()? as i64),
            "UInt8" => UnityValue::Integer(reader.read_u8()? as i64),
            "SInt16" => UnityValue::Integer(reader.read_i16()? as i64),
            "UInt16" => UnityValue::Integer(reader.read_u16()? as i64),
            "SInt32" | "int" => UnityValue::Integer(reader.read_i32()? as i64),
            "UInt32" => UnityValue::Integer(reader.read_u32()? as i64),
            "SInt64" => UnityValue::Integer(reader.read_i64()?),
            "UInt64" => UnityValue::Integer(reader.read_u64()? as i64),
            "float" => UnityValue::Float(reader.read_f32()? as f64),
            "double" => UnityValue::Float(reader.read_f64()?),
            "string" => {
                let length = reader.read_u32()? as usize;
                let bytes = reader.read_bytes(length)?;
                let string = String::from_utf8(bytes).map_err(|e| {
                    BinaryError::invalid_data(format!("Invalid UTF-8 string: {}", e))
                })?;
                reader.align()?; // Strings are aligned
                UnityValue::String(string)
            }
            "Array" => {
                // Parse array
                let size = reader.read_u32()? as usize;
                let mut array = Vec::new();

                // For now, treat array elements as raw bytes
                // A full implementation would recursively parse based on element type
                for _ in 0..size {
                    if node.children.len() > 1 {
                        // Array has element type information
                        if let Some(element_node) = node.children.get(1) {
                            let element_value = self.parse_single_value(reader, element_node)?;
                            array.push(element_value);
                        }
                    } else {
                        // Fallback: read as bytes
                        array.push(UnityValue::Integer(reader.read_u8()? as i64));
                    }
                }

                UnityValue::Array(array)
            }
            _ => {
                // Complex type or unknown type
                if node.byte_size > 0 && node.byte_size <= 1024 {
                    // Read as raw bytes for small objects
                    let bytes = reader.read_bytes(node.byte_size as usize)?;
                    UnityValue::Array(
                        bytes
                            .into_iter()
                            .map(|b| UnityValue::Integer(b as i64))
                            .collect(),
                    )
                } else {
                    // Skip large or variable-size objects
                    UnityValue::Null
                }
            }
        };

        properties.insert(node.name.clone(), value);
        Ok(())
    }

    /// Parse a single value based on TypeTree node
    fn parse_single_value(
        &self,
        reader: &mut BinaryReader,
        node: &TypeTreeNode,
    ) -> Result<UnityValue> {
        match node.type_name.as_str() {
            "bool" => Ok(UnityValue::Bool(reader.read_bool()?)),
            "SInt8" => Ok(UnityValue::Integer(reader.read_i8()? as i64)),
            "UInt8" => Ok(UnityValue::Integer(reader.read_u8()? as i64)),
            "SInt16" => Ok(UnityValue::Integer(reader.read_i16()? as i64)),
            "UInt16" => Ok(UnityValue::Integer(reader.read_u16()? as i64)),
            "SInt32" | "int" => Ok(UnityValue::Integer(reader.read_i32()? as i64)),
            "UInt32" => Ok(UnityValue::Integer(reader.read_u32()? as i64)),
            "SInt64" => Ok(UnityValue::Integer(reader.read_i64()?)),
            "UInt64" => Ok(UnityValue::Integer(reader.read_u64()? as i64)),
            "float" => Ok(UnityValue::Float(reader.read_f32()? as f64)),
            "double" => Ok(UnityValue::Float(reader.read_f64()?)),
            _ => {
                // For complex types, read as raw bytes
                if node.byte_size > 0 && node.byte_size <= 64 {
                    let bytes = reader.read_bytes(node.byte_size as usize)?;
                    Ok(UnityValue::Array(
                        bytes
                            .into_iter()
                            .map(|b| UnityValue::Integer(b as i64))
                            .collect(),
                    ))
                } else {
                    Ok(UnityValue::Null)
                }
            }
        }
    }
}

/// A Unity object with parsed data
#[derive(Debug, Clone)]
pub struct UnityObject {
    /// Object information
    pub info: ObjectInfo,
    /// Parsed Unity class data
    pub class: UnityClass,
}

impl UnityObject {
    /// Create a new Unity object
    pub fn new(info: ObjectInfo) -> Result<Self> {
        let class = info.parse_object()?;
        Ok(Self { info, class })
    }

    /// Get the object's path ID
    pub fn path_id(&self) -> i64 {
        self.info.path_id
    }

    /// Get the object's class ID
    pub fn class_id(&self) -> i32 {
        self.info.class_id
    }

    /// Get the object's class name
    pub fn class_name(&self) -> &str {
        &self.class.class_name
    }

    /// Get the object's name (if it has one)
    pub fn name(&self) -> Option<String> {
        self.class.get("m_Name").and_then(|v| match v {
            UnityValue::String(s) => Some(s.clone()),
            _ => None,
        })
    }

    /// Get a property value
    pub fn get(&self, key: &str) -> Option<&UnityValue> {
        self.class.get(key)
    }

    /// Set a property value
    pub fn set(&mut self, key: String, value: UnityValue) {
        self.class.set(key, value);
    }

    /// Check if the object has a property
    pub fn has_property(&self, key: &str) -> bool {
        self.class.has_property(key)
    }

    /// Get all property names
    pub fn property_names(&self) -> Vec<&String> {
        self.class.properties().keys().collect()
    }

    /// Get the underlying UnityClass
    pub fn as_unity_class(&self) -> &UnityClass {
        &self.class
    }

    /// Get the underlying UnityClass (mutable)
    pub fn as_unity_class_mut(&mut self) -> &mut UnityClass {
        &mut self.class
    }

    /// Try to parse this object as a GameObject
    pub fn as_gameobject(&self) -> Result<GameObject> {
        if self.class_id() != 1 {
            return Err(BinaryError::invalid_data(format!(
                "Object is not a GameObject (class_id: {})",
                self.class_id()
            )));
        }
        GameObject::from_typetree(self.class.properties())
    }

    /// Try to parse this object as a Transform
    pub fn as_transform(&self) -> Result<Transform> {
        if self.class_id() != 4 {
            return Err(BinaryError::invalid_data(format!(
                "Object is not a Transform (class_id: {})",
                self.class_id()
            )));
        }
        Transform::from_typetree(self.class.properties())
    }

    /// Check if this object is a GameObject
    pub fn is_gameobject(&self) -> bool {
        self.class_id() == 1
    }

    /// Check if this object is a Transform
    pub fn is_transform(&self) -> bool {
        self.class_id() == 4
    }

    /// Get a human-readable description of this object
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

    /// Parse object data using TypeTree (dictionary mode)
    pub fn parse_with_typetree(
        &self,
        typetree: &crate::typetree::TypeTree,
    ) -> Result<indexmap::IndexMap<String, unity_asset_core::UnityValue>> {
        let mut reader =
            crate::reader::BinaryReader::new(&self.info.data, crate::reader::ByteOrder::Little);
        let serializer = crate::typetree::TypeTreeSerializer::new(typetree);
        serializer.parse_object(&mut reader)
    }

    /// Get raw object data
    pub fn raw_data(&self) -> &[u8] {
        &self.info.data
    }

    /// Get object byte size
    pub fn byte_size(&self) -> u32 {
        self.info.byte_size
    }

    /// Get object byte start position
    pub fn byte_start(&self) -> u64 {
        self.info.byte_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_info_creation() {
        let info = ObjectInfo::new(12345, 1000, 256, 1);
        assert_eq!(info.path_id, 12345);
        assert_eq!(info.byte_start, 1000);
        assert_eq!(info.byte_size, 256);
        assert_eq!(info.class_id, 1);
    }

    #[test]
    fn test_object_info_class_name() {
        let info = ObjectInfo::new(1, 0, 0, 1);
        assert_eq!(info.class_name(), "GameObject");

        let info = ObjectInfo::new(1, 0, 0, 999999);
        assert_eq!(info.class_name(), "Class_999999");
    }

    #[test]
    fn test_object_creation() {
        let mut info = ObjectInfo::new(1, 0, 4, 1);
        info.data = vec![1, 0, 0, 0]; // Simple test data

        let result = UnityObject::new(info);
        assert!(result.is_ok());

        let object = result.unwrap();
        assert_eq!(object.path_id(), 1);
        assert_eq!(object.class_id(), 1);
        assert_eq!(object.class_name(), "GameObject");
    }
}
