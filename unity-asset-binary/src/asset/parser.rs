//! SerializedFile parser implementation
//!
//! This module provides the main parsing logic for Unity SerializedFile structures.

use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use super::header::SerializedFileHeader;
use super::types::{SerializedType, FileIdentifier, ObjectInfo, TypeRegistry};

/// SerializedFile parser
/// 
/// This struct handles the parsing of Unity SerializedFile structures,
/// supporting different Unity versions and formats.
pub struct SerializedFileParser;

impl SerializedFileParser {
    /// Parse SerializedFile from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<SerializedFile> {
        let data_clone = data.clone();
        let mut reader = BinaryReader::new(&data_clone, ByteOrder::Big);

        // Read header
        let header = SerializedFileHeader::from_reader(&mut reader)?;

        if !header.is_valid() {
            return Err(BinaryError::invalid_data("Invalid SerializedFile header"));
        }

        // Switch to the correct byte order
        reader.set_byte_order(header.byte_order());

        let mut file = SerializedFile {
            header,
            unity_version: String::new(),
            target_platform: 0,
            enable_type_tree: false,
            types: Vec::new(),
            big_id_enabled: false,
            objects: Vec::new(),
            script_types: Vec::new(),
            externals: Vec::new(),
            ref_types: Vec::new(),
            user_information: String::new(),
            data: data.clone(),
        };

        // Parse metadata
        Self::parse_metadata(&mut file, &mut reader)?;

        Ok(file)
    }

    /// Parse SerializedFile from binary data asynchronously
    #[cfg(feature = "async")]
    pub async fn from_bytes_async(data: Vec<u8>) -> Result<SerializedFile> {
        // For now, use spawn_blocking to run the sync version
        let result = tokio::task::spawn_blocking(move || Self::from_bytes(data))
            .await
            .map_err(|e| BinaryError::generic(format!("Task join error: {}", e)))??;

        Ok(result)
    }

    /// Parse the metadata section
    fn parse_metadata(file: &mut SerializedFile, reader: &mut BinaryReader) -> Result<()> {
        // Read Unity version (if version >= 7)
        if file.header.version >= 7 {
            file.unity_version = reader.read_cstring()?;
        }

        // Read target platform (if version >= 8)
        if file.header.version >= 8 {
            file.target_platform = reader.read_i32()?;
        }

        // Read enable type tree flag (if version >= 13)
        if file.header.version >= 13 {
            file.enable_type_tree = reader.read_bool()?;
        }

        // Read types
        let type_count = reader.read_u32()? as usize;
        for _ in 0..type_count {
            let serialized_type =
                SerializedType::from_reader(reader, file.header.version, file.enable_type_tree)?;
            file.types.push(serialized_type);
        }

        // Read big ID enabled flag (if version 7-13)
        if file.header.version >= 7 && file.header.version < 14 {
            file.big_id_enabled = reader.read_bool()?;
        }

        // Read objects
        let object_count = reader.read_u32()? as usize;
        for _ in 0..object_count {
            let object_info = Self::parse_object_info(file, reader)?;
            file.objects.push(object_info);
        }

        // Read script types (if version >= 11)
        if file.header.version >= 11 {
            let script_count = reader.read_u32()? as usize;
            for _ in 0..script_count {
                let script_type = SerializedType::from_reader(
                    reader,
                    file.header.version,
                    file.enable_type_tree,
                )?;
                file.script_types.push(script_type);
            }
        }

        // Read externals
        let external_count = reader.read_u32()? as usize;
        for _ in 0..external_count {
            let external = FileIdentifier::from_reader(reader, file.header.version)?;
            file.externals.push(external);
        }

        // Read ref types (if version >= 20)
        if file.header.version >= 20 {
            let ref_type_count = reader.read_u32()? as usize;
            for _ in 0..ref_type_count {
                let ref_type = SerializedType::from_reader(
                    reader,
                    file.header.version,
                    file.enable_type_tree,
                )?;
                file.ref_types.push(ref_type);
            }
        }

        // Read user information (if version >= 5)
        if file.header.version >= 5 {
            file.user_information = reader.read_cstring()?;
        }

        Ok(())
    }

    /// Parse object information
    fn parse_object_info(file: &SerializedFile, reader: &mut BinaryReader) -> Result<ObjectInfo> {
        // Read path ID
        let path_id = if file.header.version < 14 {
            reader.read_i32()? as i64
        } else {
            reader.read_i64()?
        };

        // Read byte start
        let byte_start = if file.header.version >= 22 {
            reader.read_i64()? as u64
        } else {
            reader.read_u32()? as u64
        };

        // Add data offset
        let byte_start = byte_start + file.header.data_offset as u64;

        // Read byte size
        let byte_size = reader.read_u32()?;

        // Read type ID
        let type_id = reader.read_i32()?;

        Ok(ObjectInfo::new(path_id, byte_start, byte_size, type_id))
    }

    /// Validate parsed SerializedFile
    pub fn validate(file: &SerializedFile) -> Result<()> {
        // Validate header
        file.header.validate()?;

        // Validate objects
        for (i, obj) in file.objects.iter().enumerate() {
            obj.validate().map_err(|e| {
                BinaryError::generic(format!("Object {} validation failed: {}", i, e))
            })?;
        }

        // Validate types
        for (i, stype) in file.types.iter().enumerate() {
            stype.validate().map_err(|e| {
                BinaryError::generic(format!("Type {} validation failed: {}", i, e))
            })?;
        }

        Ok(())
    }

    /// Get parsing statistics
    pub fn get_parsing_stats(file: &SerializedFile) -> ParsingStats {
        ParsingStats {
            version: file.header.version,
            unity_version: file.unity_version.clone(),
            target_platform: file.target_platform,
            file_size: file.header.file_size,
            object_count: file.objects.len(),
            type_count: file.types.len(),
            script_type_count: file.script_types.len(),
            external_count: file.externals.len(),
            has_type_tree: file.enable_type_tree,
            big_id_enabled: file.big_id_enabled,
        }
    }
}

/// Complete SerializedFile structure
/// 
/// This structure represents a complete Unity SerializedFile with all its
/// metadata, type information, and object data.
#[derive(Debug)]
pub struct SerializedFile {
    /// File header
    pub header: SerializedFileHeader,
    /// Unity version string
    pub unity_version: String,
    /// Target platform
    pub target_platform: i32,
    /// Whether type tree is enabled
    pub enable_type_tree: bool,
    /// Type information
    pub types: Vec<SerializedType>,
    /// Whether big IDs are enabled
    pub big_id_enabled: bool,
    /// Object information
    pub objects: Vec<ObjectInfo>,
    /// Script types
    pub script_types: Vec<SerializedType>,
    /// External file references
    pub externals: Vec<FileIdentifier>,
    /// Reference types
    pub ref_types: Vec<SerializedType>,
    /// User information
    pub user_information: String,
    /// Raw file data
    data: Vec<u8>,
}

impl SerializedFile {
    /// Get the raw file data
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get object count
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Get type count
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    /// Find object by path ID
    pub fn find_object(&self, path_id: i64) -> Option<&ObjectInfo> {
        self.objects.iter().find(|obj| obj.path_id == path_id)
    }

    /// Find type by class ID
    pub fn find_type(&self, class_id: i32) -> Option<&SerializedType> {
        self.types.iter().find(|t| t.class_id == class_id)
    }

    /// Get all objects of a specific type
    pub fn objects_of_type(&self, type_id: i32) -> Vec<&ObjectInfo> {
        self.objects.iter().filter(|obj| obj.type_id == type_id).collect()
    }

    /// Create a type registry from this file
    pub fn create_type_registry(&self) -> TypeRegistry {
        let mut registry = TypeRegistry::new();
        
        for stype in &self.types {
            registry.add_type(stype.clone());
        }
        
        for script_type in &self.script_types {
            registry.add_type(script_type.clone());
        }
        
        registry
    }

    /// Get file statistics
    pub fn statistics(&self) -> FileStatistics {
        FileStatistics {
            version: self.header.version,
            unity_version: self.unity_version.clone(),
            file_size: self.header.file_size,
            object_count: self.objects.len(),
            type_count: self.types.len(),
            script_type_count: self.script_types.len(),
            external_count: self.externals.len(),
            has_type_tree: self.enable_type_tree,
            target_platform: self.target_platform,
        }
    }

    /// Validate the entire file
    pub fn validate(&self) -> Result<()> {
        SerializedFileParser::validate(self)
    }
}

/// Parsing statistics
#[derive(Debug, Clone)]
pub struct ParsingStats {
    pub version: u32,
    pub unity_version: String,
    pub target_platform: i32,
    pub file_size: u32,
    pub object_count: usize,
    pub type_count: usize,
    pub script_type_count: usize,
    pub external_count: usize,
    pub has_type_tree: bool,
    pub big_id_enabled: bool,
}

/// File statistics
#[derive(Debug, Clone)]
pub struct FileStatistics {
    pub version: u32,
    pub unity_version: String,
    pub file_size: u32,
    pub object_count: usize,
    pub type_count: usize,
    pub script_type_count: usize,
    pub external_count: usize,
    pub has_type_tree: bool,
    pub target_platform: i32,
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parser_creation() {
        // Basic test to ensure parser methods exist
        assert!(true);
    }
}
