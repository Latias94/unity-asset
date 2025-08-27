//! Asset type definitions
//!
//! This module defines the core data structures for Unity asset processing.

use crate::error::{BinaryError, Result};
use crate::reader::BinaryReader;
use crate::typetree::{TypeTree, TypeTreeParser};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Type information for Unity objects
/// 
/// Contains metadata about Unity object types including class information,
/// type trees, and script references.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedType {
    /// Unity class ID
    pub class_id: i32,
    /// Whether this type is stripped
    pub is_stripped_type: bool,
    /// Script type index (for MonoBehaviour)
    pub script_type_index: Option<i16>,
    /// Type tree for this type
    pub type_tree: TypeTree,
    /// Script ID hash
    pub script_id: [u8; 16],
    /// Old type hash
    pub old_type_hash: [u8; 16],
    /// Type dependencies
    pub type_dependencies: Vec<i32>,
    /// Class name
    pub class_name: String,
    /// Namespace
    pub namespace: String,
    /// Assembly name
    pub assembly_name: String,
}

impl SerializedType {
    /// Create a new SerializedType
    pub fn new(class_id: i32) -> Self {
        Self {
            class_id,
            is_stripped_type: false,
            script_type_index: None,
            type_tree: TypeTree::new(),
            script_id: [0; 16],
            old_type_hash: [0; 16],
            type_dependencies: Vec::new(),
            class_name: String::new(),
            namespace: String::new(),
            assembly_name: String::new(),
        }
    }

    /// Parse SerializedType from binary data
    pub fn from_reader(
        reader: &mut BinaryReader,
        version: u32,
        enable_type_tree: bool,
    ) -> Result<Self> {
        let class_id = reader.read_i32()?;
        let mut serialized_type = Self::new(class_id);

        if version >= 16 {
            serialized_type.is_stripped_type = reader.read_bool()?;
        }

        if version >= 17 {
            let script_type_index = reader.read_i16()?;
            serialized_type.script_type_index = Some(script_type_index);
        }

        if version >= 13 {
            // Based on unity-rs logic: check conditions for script_id
            let should_read_script_id = if version < 16 {
                class_id < 0
            } else {
                class_id == 114 // MonoBehaviour
            };

            if should_read_script_id {
                // Read script ID
                let script_id_bytes = reader.read_bytes(16)?;
                serialized_type.script_id.copy_from_slice(&script_id_bytes);
            }

            // Always read old type hash for version >= 13
            let old_type_hash_bytes = reader.read_bytes(16)?;
            serialized_type
                .old_type_hash
                .copy_from_slice(&old_type_hash_bytes);
        }

        if enable_type_tree {
            // Use blob format for version >= 12 or version == 10 (like unity-rs)
            if version >= 12 || version == 10 {
                serialized_type.type_tree = TypeTreeParser::from_reader_blob(reader, version)?;
            } else {
                serialized_type.type_tree = TypeTreeParser::from_reader(reader, version)?;
            }
        }

        Ok(serialized_type)
    }

    /// Check if this is a script type (MonoBehaviour)
    pub fn is_script_type(&self) -> bool {
        self.class_id == 114 || self.script_type_index.is_some()
    }

    /// Check if this type has a TypeTree
    pub fn has_type_tree(&self) -> bool {
        !self.type_tree.is_empty()
    }

    /// Get the type name
    pub fn type_name(&self) -> String {
        if !self.class_name.is_empty() {
            self.class_name.clone()
        } else {
            format!("Class_{}", self.class_id)
        }
    }

    /// Get full type name including namespace
    pub fn full_type_name(&self) -> String {
        if !self.namespace.is_empty() {
            format!("{}.{}", self.namespace, self.type_name())
        } else {
            self.type_name()
        }
    }

    /// Validate the serialized type
    pub fn validate(&self) -> Result<()> {
        if self.class_id == 0 {
            return Err(BinaryError::invalid_data("Class ID cannot be zero"));
        }

        if self.is_script_type() && self.script_id == [0; 16] {
            return Err(BinaryError::invalid_data(
                "Script type must have valid script ID"
            ));
        }

        Ok(())
    }
}

/// External reference to another Unity file
/// 
/// Represents a reference to an asset in another Unity file,
/// used for cross-file asset dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileIdentifier {
    /// GUID of the referenced file
    pub guid: [u8; 16],
    /// Type of the reference
    pub type_: i32,
    /// Path to the referenced file
    pub path: String,
}

impl FileIdentifier {
    /// Parse FileIdentifier from binary data
    pub fn from_reader(reader: &mut BinaryReader, _version: u32) -> Result<Self> {
        let mut guid = [0u8; 16];
        let guid_bytes = reader.read_bytes(16)?;
        guid.copy_from_slice(&guid_bytes);

        let type_ = reader.read_i32()?;
        let path = reader.read_aligned_string()?;

        Ok(Self { guid, type_, path })
    }

    /// Create a new FileIdentifier
    pub fn new(guid: [u8; 16], type_: i32, path: String) -> Self {
        Self { guid, type_, path }
    }

    /// Check if this is a valid file identifier
    pub fn is_valid(&self) -> bool {
        self.guid != [0; 16] || !self.path.is_empty()
    }

    /// Get GUID as string
    pub fn guid_string(&self) -> String {
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.guid[0], self.guid[1], self.guid[2], self.guid[3],
            self.guid[4], self.guid[5], self.guid[6], self.guid[7],
            self.guid[8], self.guid[9], self.guid[10], self.guid[11],
            self.guid[12], self.guid[13], self.guid[14], self.guid[15]
        )
    }
}

impl Default for FileIdentifier {
    fn default() -> Self {
        Self {
            guid: [0; 16],
            type_: 0,
            path: String::new(),
        }
    }
}

/// Object information within a SerializedFile
/// 
/// Contains metadata about individual Unity objects including
/// their location, type, and path ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    /// Path ID of the object (unique within file)
    pub path_id: i64,
    /// Offset of object data in the file
    pub byte_start: u64,
    /// Size of object data
    pub byte_size: u32,
    /// Type ID of the object
    pub type_id: i32,
    /// Object data
    pub data: Vec<u8>,
}

impl ObjectInfo {
    /// Create a new ObjectInfo
    pub fn new(path_id: i64, byte_start: u64, byte_size: u32, type_id: i32) -> Self {
        Self {
            path_id,
            byte_start,
            byte_size,
            type_id,
            data: Vec::new(),
        }
    }

    /// Check if object data is loaded
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }

    /// Get the end offset of this object
    pub fn byte_end(&self) -> u64 {
        self.byte_start + self.byte_size as u64
    }

    /// Validate object info
    pub fn validate(&self) -> Result<()> {
        if self.path_id == 0 {
            return Err(BinaryError::invalid_data("Path ID cannot be zero"));
        }

        if self.byte_size == 0 {
            return Err(BinaryError::invalid_data("Byte size cannot be zero"));
        }

        if self.type_id == 0 {
            return Err(BinaryError::invalid_data("Type ID cannot be zero"));
        }

        Ok(())
    }
}

/// Type registry for managing SerializedTypes
/// 
/// Provides efficient lookup and management of type information
/// within a SerializedFile.
#[derive(Debug, Clone, Default)]
pub struct TypeRegistry {
    types: HashMap<i32, SerializedType>,
    script_types: HashMap<i16, SerializedType>,
}

impl TypeRegistry {
    /// Create a new type registry
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
            script_types: HashMap::new(),
        }
    }

    /// Add a type to the registry
    pub fn add_type(&mut self, serialized_type: SerializedType) {
        let class_id = serialized_type.class_id;
        
        // Add to script types if applicable
        if let Some(script_index) = serialized_type.script_type_index {
            self.script_types.insert(script_index, serialized_type.clone());
        }
        
        self.types.insert(class_id, serialized_type);
    }

    /// Get a type by class ID
    pub fn get_type(&self, class_id: i32) -> Option<&SerializedType> {
        self.types.get(&class_id)
    }

    /// Get a script type by index
    pub fn get_script_type(&self, script_index: i16) -> Option<&SerializedType> {
        self.script_types.get(&script_index)
    }

    /// Get all class IDs
    pub fn class_ids(&self) -> Vec<i32> {
        self.types.keys().copied().collect()
    }

    /// Get all script type indices
    pub fn script_indices(&self) -> Vec<i16> {
        self.script_types.keys().copied().collect()
    }

    /// Check if a class ID is registered
    pub fn has_type(&self, class_id: i32) -> bool {
        self.types.contains_key(&class_id)
    }

    /// Check if a script index is registered
    pub fn has_script_type(&self, script_index: i16) -> bool {
        self.script_types.contains_key(&script_index)
    }

    /// Get the number of registered types
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Clear all types
    pub fn clear(&mut self) {
        self.types.clear();
        self.script_types.clear();
    }

    /// Get types by predicate
    pub fn find_types<F>(&self, predicate: F) -> Vec<&SerializedType>
    where
        F: Fn(&SerializedType) -> bool,
    {
        self.types.values().filter(|t| predicate(t)).collect()
    }

    /// Get all script types
    pub fn script_types(&self) -> Vec<&SerializedType> {
        self.script_types.values().collect()
    }

    /// Get all non-script types
    pub fn non_script_types(&self) -> Vec<&SerializedType> {
        self.types.values().filter(|t| !t.is_script_type()).collect()
    }
}

/// Unity class ID constants
pub mod class_ids {
    pub const OBJECT: i32 = 1;
    pub const COMPONENT: i32 = 2;
    pub const BEHAVIOUR: i32 = 3;
    pub const UNITY_ENGINE_OBJECT: i32 = 4;
    pub const GAME_OBJECT: i32 = 1;
    pub const TRANSFORM: i32 = 4;
    pub const MONO_BEHAVIOUR: i32 = 114;
    pub const TEXTURE_2D: i32 = 28;
    pub const SPRITE: i32 = 213;
    pub const MESH: i32 = 43;
    pub const AUDIO_CLIP: i32 = 83;
    pub const MATERIAL: i32 = 21;
    pub const SHADER: i32 = 48;
    pub const ANIMATION_CLIP: i32 = 74;
    pub const ANIMATOR_CONTROLLER: i32 = 91;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialized_type_creation() {
        let stype = SerializedType::new(114);
        assert_eq!(stype.class_id, 114);
        assert!(stype.is_script_type());
    }

    #[test]
    fn test_file_identifier_guid() {
        let guid = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let file_id = FileIdentifier::new(guid, 0, "test.unity".to_string());
        let guid_str = file_id.guid_string();
        assert!(guid_str.contains("01020304"));
    }

    #[test]
    fn test_type_registry() {
        let mut registry = TypeRegistry::new();
        let stype = SerializedType::new(28); // Texture2D
        
        registry.add_type(stype);
        assert!(registry.has_type(28));
        assert_eq!(registry.len(), 1);
    }
}
