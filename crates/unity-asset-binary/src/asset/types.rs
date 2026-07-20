//! Asset type definitions
//!
//! This module defines the core data structures for Unity asset processing.

use super::format::{ExternalEncoding, PathIdEncoding, SerializedFileFormat};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryInput, BinaryReader, not_enough_data_u64};
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
    /// Script type index (for MonoBehaviour / ref types); `-1` means not a script type.
    pub script_type_index: i16,
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
            script_type_index: -1,
            type_tree: TypeTree::new(),
            script_id: [0; 16],
            old_type_hash: [0; 16],
            type_dependencies: Vec::new(),
            class_name: String::new(),
            namespace: String::new(),
            assembly_name: String::new(),
        }
    }

    pub(crate) fn from_input<I: BinaryInput + ?Sized>(
        input: &mut I,
        format: SerializedFileFormat,
        enable_type_tree: bool,
        is_ref_type: bool,
    ) -> Result<Self> {
        let class_id = input.read_i32()?;
        let mut serialized_type = Self::new(class_id);

        if format.serialized_types_have_stripped_flag() {
            serialized_type.is_stripped_type = input.read_bool()?;
        }

        if format.serialized_types_have_script_type_index() {
            serialized_type.script_type_index = input.read_i16()?;
        }

        if format.serialized_types_have_hashes() {
            if format.serialized_type_has_script_id(
                class_id,
                serialized_type.script_type_index,
                is_ref_type,
            ) {
                let script_id_bytes = input.read_bytes(16)?;
                serialized_type.script_id.copy_from_slice(&script_id_bytes);
            }

            let old_type_hash_bytes = input.read_bytes(16)?;
            serialized_type
                .old_type_hash
                .copy_from_slice(&old_type_hash_bytes);
        }

        if enable_type_tree {
            serialized_type.type_tree = TypeTreeParser::from_input_with_format(input, format)?;

            if is_ref_type && format.has_ref_type_names() {
                serialized_type.class_name =
                    input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;
                serialized_type.namespace =
                    input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;
                serialized_type.assembly_name =
                    input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;
            } else if !is_ref_type && format.has_type_dependencies() {
                serialized_type.type_dependencies = read_i32_array(input)?;
            }
        }

        Ok(serialized_type)
    }

    /// Check if this is a script type (MonoBehaviour)
    pub fn is_script_type(&self) -> bool {
        self.class_id == 114 || self.script_type_index >= 0
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
                "Script type must have valid script ID",
            ));
        }

        Ok(())
    }

    /// Validates that retained wire fields are representable by the selected format.
    pub fn validate_for_format(&self, format: SerializedFileFormat) -> Result<()> {
        if self.class_id == 0 {
            return Err(BinaryError::invalid_data("Class ID cannot be zero"));
        }
        if !format.serialized_types_have_stripped_flag() && self.is_stripped_type {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{} cannot encode a stripped SerializedType flag",
                format.version()
            )));
        }
        if !format.serialized_types_have_script_type_index() && self.script_type_index != -1 {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{} cannot encode a SerializedType script index",
                format.version()
            )));
        }
        if !format.serialized_types_have_hashes()
            && (self.script_id != [0; 16] || self.old_type_hash != [0; 16])
        {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{} cannot encode SerializedType hashes",
                format.version()
            )));
        }
        if !format.has_type_dependencies() && !self.type_dependencies.is_empty() {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{} cannot encode type dependencies",
                format.version()
            )));
        }
        if !self.type_tree.is_empty() && self.type_tree.version != format.version() {
            return Err(BinaryError::invalid_data(format!(
                "TypeTree version {} does not match SerializedFile v{}",
                self.type_tree.version,
                format.version()
            )));
        }
        Ok(())
    }
}

fn read_i32_array(input: &mut (impl BinaryInput + ?Sized)) -> Result<Vec<i32>> {
    let signed_count = input.read_i32()?;
    let count = u64::try_from(signed_count)
        .map_err(|_| BinaryError::invalid_data(format!("Negative array length: {signed_count}")))?;
    let byte_count = count
        .checked_mul(std::mem::size_of::<i32>() as u64)
        .ok_or_else(|| BinaryError::invalid_data("i32 array byte length overflow"))?;
    if byte_count > input.remaining() {
        return Err(not_enough_data_u64(byte_count, input.remaining()));
    }
    input.consume_entries(count)?;
    let count = usize::try_from(count)
        .map_err(|_| BinaryError::memory_error("i32 array length does not fit in usize"))?;
    let mut values = Vec::new();
    values.try_reserve_exact(count).map_err(|error| {
        BinaryError::memory_error(format!("Failed to reserve {count} i32 values: {error}"))
    })?;
    for _ in 0..count {
        values.push(input.read_i32()?);
    }
    Ok(values)
}

/// External reference to another Unity file
///
/// Represents a reference to an asset in another Unity file,
/// used for cross-file asset dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FileIdentifier {
    /// Temporary empty string field for version >= 6.
    pub temp_empty: String,
    /// GUID of the referenced file
    pub guid: [u8; 16],
    /// Type of the reference
    pub type_: i32,
    /// Path to the referenced file
    pub path: String,
}

impl FileIdentifier {
    pub(crate) fn from_input<I: BinaryInput + ?Sized>(
        input: &mut I,
        format: SerializedFileFormat,
    ) -> Result<Self> {
        let encoding = format.external_encoding();
        let temp_empty = match encoding {
            ExternalEncoding::AssetPathGuidAndType => {
                input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?
            }
            ExternalEncoding::PathOnly | ExternalEncoding::GuidAndType => String::new(),
        };

        let mut guid = [0u8; 16];
        let mut type_ = 0i32;

        if matches!(
            encoding,
            ExternalEncoding::GuidAndType | ExternalEncoding::AssetPathGuidAndType
        ) {
            let guid_bytes = input.read_bytes(16)?;
            guid.copy_from_slice(&guid_bytes);
            type_ = input.read_i32()?;
        }

        let path = input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;

        Ok(Self {
            temp_empty,
            guid,
            type_,
            path,
        })
    }

    /// Create a new FileIdentifier
    pub fn new(guid: [u8; 16], type_: i32, path: String) -> Self {
        Self {
            temp_empty: String::new(),
            guid,
            type_,
            path,
        }
    }

    /// Check if this is a valid file identifier
    pub fn is_valid(&self) -> bool {
        self.guid != [0; 16] || !self.path.is_empty()
    }

    /// Get GUID as string
    pub fn guid_string(&self) -> String {
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.guid[0],
            self.guid[1],
            self.guid[2],
            self.guid[3],
            self.guid[4],
            self.guid[5],
            self.guid[6],
            self.guid[7],
            self.guid[8],
            self.guid[9],
            self.guid[10],
            self.guid[11],
            self.guid[12],
            self.guid[13],
            self.guid[14],
            self.guid[15]
        )
    }
}

/// Exact meaning and bit pattern of the object table's 32-bit type reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectTypeReference {
    /// Standalone object parsed outside a SerializedFile object table.
    ///
    /// This state has no wire representation and must be rejected by SerializedFile encoders.
    StandaloneClass { class_id: i32 },
    /// Legacy type ID followed by an independently stored 16-bit class-ID bit pattern.
    Legacy {
        raw_type_id: i32,
        class_id_bits: u16,
    },
    /// Format 16 transition whose raw value is resolved with index-first wire semantics.
    TransitionalV16 { raw: i32 },
    /// Validated index into the SerializedType table.
    SerializedTypeIndex { index: u32 },
}

impl ObjectTypeReference {
    /// Returns the original 32-bit value stored in the object table.
    pub fn raw_value(self) -> Result<i32> {
        match self {
            Self::StandaloneClass { class_id } => Err(BinaryError::invalid_data(format!(
                "Standalone class ID {class_id} has no SerializedFile wire representation"
            ))),
            Self::Legacy { raw_type_id, .. } => Ok(raw_type_id),
            Self::TransitionalV16 { raw } => Ok(raw),
            Self::SerializedTypeIndex { index } => i32::try_from(index).map_err(|_| {
                BinaryError::invalid_data(format!(
                    "SerializedType index {index} does not fit the i32 wire field"
                ))
            }),
        }
    }

    pub const fn legacy_class_id_bits(self) -> Option<u16> {
        match self {
            Self::Legacy { class_id_bits, .. } => Some(class_id_bits),
            Self::StandaloneClass { .. }
            | Self::TransitionalV16 { .. }
            | Self::SerializedTypeIndex { .. } => None,
        }
    }
}

/// Version-specific object-table fields retained exactly as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectMetadata {
    Destroyed { value: u16 },
    ScriptTypeIndex { index: i16 },
    ScriptTypeIndexAndStripped { index: i16, stripped: u8 },
    None,
}

impl ObjectMetadata {
    pub const fn destroyed(self) -> Option<u16> {
        match self {
            Self::Destroyed { value } => Some(value),
            _ => None,
        }
    }

    pub const fn script_type_index(self) -> Option<i16> {
        match self {
            Self::ScriptTypeIndex { index } | Self::ScriptTypeIndexAndStripped { index, .. } => {
                Some(index)
            }
            Self::Destroyed { .. } | Self::None => None,
        }
    }

    pub const fn stripped_raw(self) -> Option<u8> {
        match self {
            Self::ScriptTypeIndexAndStripped { stripped, .. } => Some(stripped),
            _ => None,
        }
    }
}

/// Object information within a SerializedFile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectInfo {
    path_id: i64,
    byte_start: u64,
    byte_size: u32,
    type_reference: ObjectTypeReference,
    class_id: i32,
    serialized_type_index: Option<u32>,
    metadata: ObjectMetadata,
    loaded_data: Option<Vec<u8>>,
}

impl ObjectInfo {
    pub(crate) fn from_wire(
        path_id: i64,
        byte_start: u64,
        byte_size: u32,
        type_reference: ObjectTypeReference,
        class_id: i32,
        serialized_type_index: Option<u32>,
        metadata: ObjectMetadata,
    ) -> Self {
        Self {
            path_id,
            byte_start,
            byte_size,
            type_reference,
            class_id,
            serialized_type_index,
            metadata,
            loaded_data: None,
        }
    }

    /// Constructs metadata for standalone object parsing outside a SerializedFile object table.
    ///
    /// Standalone metadata intentionally has no SerializedFile wire representation.
    pub fn for_standalone_class(
        path_id: i64,
        byte_start: u64,
        byte_size: u32,
        class_id: i32,
    ) -> Result<Self> {
        let object = Self {
            path_id,
            byte_start,
            byte_size,
            type_reference: ObjectTypeReference::StandaloneClass { class_id },
            class_id,
            serialized_type_index: None,
            metadata: ObjectMetadata::None,
            loaded_data: None,
        };
        object.validate()?;
        Ok(object)
    }

    pub const fn path_id(&self) -> i64 {
        self.path_id
    }

    pub const fn byte_start(&self) -> u64 {
        self.byte_start
    }

    pub const fn byte_size(&self) -> u32 {
        self.byte_size
    }

    pub const fn type_reference(&self) -> ObjectTypeReference {
        self.type_reference
    }

    pub const fn class_id(&self) -> i32 {
        self.class_id
    }

    pub const fn serialized_type_index(&self) -> Option<u32> {
        self.serialized_type_index
    }

    pub const fn metadata(&self) -> ObjectMetadata {
        self.metadata
    }

    /// Returns object bytes loaded independently of the backing SerializedFile.
    ///
    /// `Some(&[])` is a loaded zero-byte payload. `None` means callers may fall back to the
    /// object's backing SerializedFile range.
    pub fn loaded_data(&self) -> Option<&[u8]> {
        self.loaded_data.as_deref()
    }

    pub fn set_data(&mut self, data: Vec<u8>) {
        self.loaded_data = Some(data);
    }

    pub fn clear_data(&mut self) {
        self.loaded_data = None;
    }

    pub(crate) fn clone_without_loaded_data(&self) -> Self {
        Self {
            path_id: self.path_id,
            byte_start: self.byte_start,
            byte_size: self.byte_size,
            type_reference: self.type_reference,
            class_id: self.class_id,
            serialized_type_index: self.serialized_type_index,
            metadata: self.metadata,
            loaded_data: None,
        }
    }

    /// Returns the checked absolute end offset of this object's payload.
    pub fn byte_end(&self) -> Result<u64> {
        self.byte_start
            .checked_add(u64::from(self.byte_size))
            .ok_or_else(|| BinaryError::invalid_data("Object byte range overflow"))
    }

    /// Validate object info
    pub fn validate(&self) -> Result<()> {
        if self.path_id == 0 {
            return Err(BinaryError::invalid_data("Path ID cannot be zero"));
        }

        if self.class_id == 0 {
            return Err(BinaryError::invalid_data("Type ID cannot be zero"));
        }

        match self.type_reference {
            ObjectTypeReference::StandaloneClass { class_id } => {
                if class_id != self.class_id {
                    return Err(BinaryError::invalid_data(
                        "Standalone object type reference and class ID disagree",
                    ));
                }
                if self.serialized_type_index.is_some() || self.metadata != ObjectMetadata::None {
                    return Err(BinaryError::invalid_data(
                        "Standalone objects cannot carry SerializedFile type or metadata fields",
                    ));
                }
            }
            ObjectTypeReference::SerializedTypeIndex { index } => {
                if self.serialized_type_index != Some(index) {
                    return Err(BinaryError::invalid_data(
                        "Object type reference and resolved SerializedType index disagree",
                    ));
                }
            }
            ObjectTypeReference::Legacy { .. } | ObjectTypeReference::TransitionalV16 { .. } => {}
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
        if serialized_type.script_type_index >= 0 {
            self.script_types
                .insert(serialized_type.script_type_index, serialized_type.clone());
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
        self.types
            .values()
            .filter(|t| !t.is_script_type())
            .collect()
    }
}

/// Unity class ID constants (single source of truth: `unity-asset-core`)
pub use unity_asset_core::class_ids;

/// Script type reference (UnityPy: LocalSerializedObjectIdentifier)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSerializedObjectIdentifier {
    pub local_serialized_file_index: i32,
    pub local_identifier_in_file: i64,
}

impl LocalSerializedObjectIdentifier {
    pub(crate) fn from_input<I: BinaryInput + ?Sized>(
        input: &mut I,
        format: SerializedFileFormat,
    ) -> Result<Self> {
        let local_serialized_file_index = input.read_i32()?;
        let local_identifier_in_file = match format.path_id_encoding() {
            PathIdEncoding::I32 | PathIdEncoding::BigIdFlag => i64::from(input.read_i32()?),
            PathIdEncoding::AlignedI64 => {
                input.align()?;
                input.read_i64()?
            }
        };
        Ok(Self {
            local_serialized_file_index,
            local_identifier_in_file,
        })
    }
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

    #[test]
    fn zero_byte_object_data_retains_loaded_state() {
        let mut object = ObjectInfo::for_standalone_class(1, 0, 0, 1).unwrap();
        assert_eq!(object.loaded_data(), None);

        object.set_data(Vec::new());
        assert_eq!(object.loaded_data(), Some([].as_slice()));

        object.clear_data();
        assert_eq!(object.loaded_data(), None);
    }

    #[test]
    fn standalone_object_preserves_full_i32_class_id_without_wire_fields() {
        let object = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::SPRITE_ATLAS)
            .expect("standalone objects accept every valid Unity class ID");

        assert_eq!(object.class_id(), class_ids::SPRITE_ATLAS);
        assert_eq!(
            object.type_reference(),
            ObjectTypeReference::StandaloneClass {
                class_id: class_ids::SPRITE_ATLAS,
            }
        );
        assert!(object.type_reference().raw_value().is_err());
    }
}
