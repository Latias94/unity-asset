//! Unity asset processing module
//!
//! This module provides comprehensive Unity asset processing capabilities,
//! organized following UnityPy and unity-rs best practices.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `header` - SerializedFile header parsing and validation
//! - `types` - Core data structures (SerializedType, FileIdentifier, etc.)
//! - `parser` - Main parsing logic for SerializedFile structures
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::asset::{SerializedFileParser, SerializedFile};
//!
//! // Parse SerializedFile from binary data
//! let data = std::fs::read("example.assets")?;
//! let serialized_file = SerializedFileParser::from_bytes(data)?;
//!
//! // Access objects and types
//! println!("Object count: {}", serialized_file.object_count());
//! println!("Type count: {}", serialized_file.type_count());
//!
//! // Find specific objects
//! let textures = serialized_file.objects_of_type(28); // Texture2D
//! # Ok::<(), unity_asset_binary::error::BinaryError>(())
//! ```

pub mod header;
pub mod parser;
pub mod types;

// Re-export main types for easy access
pub use header::{HeaderFormatInfo, HeaderValidation, SerializedFileHeader, validate_header};
pub use parser::{FileStatistics, ParsingStats, SerializedFile, SerializedFileParser};
pub use types::{FileIdentifier, ObjectInfo, SerializedType, TypeRegistry, class_ids};

// Legacy compatibility - Asset is an alias for SerializedFile
pub type Asset = SerializedFile;

/// Main asset processing facade
///
/// This struct provides a high-level interface for asset processing,
/// combining parsing and type management functionality.
pub struct AssetProcessor {
    file: Option<SerializedFile>,
}

impl AssetProcessor {
    /// Create a new asset processor
    pub fn new() -> Self {
        Self { file: None }
    }

    /// Parse SerializedFile from binary data
    pub fn parse_from_bytes(&mut self, data: Vec<u8>) -> crate::error::Result<()> {
        let file = SerializedFileParser::from_bytes(data)?;
        self.file = Some(file);
        Ok(())
    }

    /// Parse SerializedFile from file path
    pub fn parse_from_file<P: AsRef<std::path::Path>>(
        &mut self,
        path: P,
    ) -> crate::error::Result<()> {
        let data = std::fs::read(path).map_err(|e| {
            crate::error::BinaryError::generic(format!("Failed to read file: {}", e))
        })?;
        self.parse_from_bytes(data)
    }

    /// Parse SerializedFile asynchronously
    #[cfg(feature = "async")]
    pub async fn parse_from_bytes_async(&mut self, data: Vec<u8>) -> crate::error::Result<()> {
        let file = SerializedFileParser::from_bytes_async(data).await?;
        self.file = Some(file);
        Ok(())
    }

    /// Get the loaded SerializedFile
    pub fn file(&self) -> Option<&SerializedFile> {
        self.file.as_ref()
    }

    /// Get mutable access to the loaded SerializedFile
    pub fn file_mut(&mut self) -> Option<&mut SerializedFile> {
        self.file.as_mut()
    }

    /// Get objects of a specific type
    pub fn objects_of_type(&self, type_id: i32) -> Vec<&ObjectInfo> {
        self.file
            .as_ref()
            .map(|f| f.objects_of_type(type_id))
            .unwrap_or_default()
    }

    /// Find object by path ID
    pub fn find_object(&self, path_id: i64) -> Option<&ObjectInfo> {
        self.file.as_ref().and_then(|f| f.find_object(path_id))
    }

    /// Find type by class ID
    pub fn find_type(&self, class_id: i32) -> Option<&SerializedType> {
        self.file.as_ref().and_then(|f| f.find_type(class_id))
    }

    /// Get file statistics
    pub fn statistics(&self) -> Option<FileStatistics> {
        self.file.as_ref().map(|f| f.statistics())
    }

    /// Validate the loaded file
    pub fn validate(&self) -> crate::error::Result<()> {
        self.file
            .as_ref()
            .ok_or_else(|| crate::error::BinaryError::generic("No file loaded"))?
            .validate()
    }

    /// Create a type registry from the loaded file
    pub fn create_type_registry(&self) -> Option<TypeRegistry> {
        self.file.as_ref().map(|f| f.create_type_registry())
    }

    /// Clear the loaded file
    pub fn clear(&mut self) {
        self.file = None;
    }

    /// Check if a file is loaded
    pub fn has_file(&self) -> bool {
        self.file.is_some()
    }

    /// Get Unity version
    pub fn unity_version(&self) -> Option<&str> {
        self.file.as_ref().map(|f| f.unity_version.as_str())
    }

    /// Get file format version
    pub fn format_version(&self) -> Option<u32> {
        self.file.as_ref().map(|f| f.header.version)
    }

    /// Get target platform
    pub fn target_platform(&self) -> Option<i32> {
        self.file.as_ref().map(|f| f.target_platform)
    }
}

impl Default for AssetProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience functions for common operations

/// Create an asset processor with default settings
pub fn create_processor() -> AssetProcessor {
    AssetProcessor::default()
}

/// Parse SerializedFile from binary data
pub fn parse_serialized_file(data: Vec<u8>) -> crate::error::Result<SerializedFile> {
    SerializedFileParser::from_bytes(data)
}

/// Parse SerializedFile from file path
pub fn parse_serialized_file_from_path<P: AsRef<std::path::Path>>(
    path: P,
) -> crate::error::Result<SerializedFile> {
    let data = std::fs::read(path)
        .map_err(|e| crate::error::BinaryError::generic(format!("Failed to read file: {}", e)))?;
    SerializedFileParser::from_bytes(data)
}

/// Parse SerializedFile asynchronously
#[cfg(feature = "async")]
pub async fn parse_serialized_file_async(data: Vec<u8>) -> crate::error::Result<SerializedFile> {
    SerializedFileParser::from_bytes_async(data).await
}

/// Get file information without full parsing
pub fn get_file_info<P: AsRef<std::path::Path>>(path: P) -> crate::error::Result<AssetFileInfo> {
    let data = std::fs::read(&path)
        .map_err(|e| crate::error::BinaryError::generic(format!("Failed to read file: {}", e)))?;

    // Parse just the header and basic metadata
    let mut reader = crate::reader::BinaryReader::new(&data, crate::reader::ByteOrder::Big);
    let header = SerializedFileHeader::from_reader(&mut reader)?;

    reader.set_byte_order(header.byte_order());

    // Read Unity version if available
    let unity_version = if header.version >= 7 {
        reader.read_cstring().unwrap_or_default()
    } else {
        String::new()
    };

    // Read target platform if available
    let target_platform = if header.version >= 8 {
        reader.read_i32().unwrap_or(0)
    } else {
        0
    };

    Ok(AssetFileInfo {
        path: path.as_ref().to_string_lossy().to_string(),
        format_version: header.version,
        unity_version,
        target_platform,
        file_size: header.file_size,
        is_big_endian: header.endian != 0,
        supports_type_tree: header.supports_type_trees(),
    })
}

/// Check if a file is a valid Unity SerializedFile
pub fn is_valid_serialized_file<P: AsRef<std::path::Path>>(path: P) -> bool {
    match std::fs::read(path) {
        Ok(data) => {
            if data.len() < 20 {
                return false;
            }

            let mut reader = crate::reader::BinaryReader::new(&data, crate::reader::ByteOrder::Big);
            match SerializedFileHeader::from_reader(&mut reader) {
                Ok(header) => header.is_valid(),
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

/// Asset file information summary
#[derive(Debug, Clone)]
pub struct AssetFileInfo {
    pub path: String,
    pub format_version: u32,
    pub unity_version: String,
    pub target_platform: i32,
    pub file_size: u32,
    pub is_big_endian: bool,
    pub supports_type_tree: bool,
}

/// Get supported Unity versions
pub fn get_supported_versions() -> Vec<u32> {
    (5..=50).collect() // Support Unity 5.x to 2023.x (approximately)
}

/// Check if a Unity version is supported
pub fn is_version_supported(version: u32) -> bool {
    version >= 5 && version <= 50
}

/// Get recommended parsing options for a Unity version
pub fn get_parsing_options(version: u32) -> ParsingOptions {
    ParsingOptions {
        enable_type_tree: version >= 13,
        use_big_ids: version >= 14,
        supports_script_types: version >= 11,
        supports_ref_types: version >= 20,
        uses_extended_format: version >= 22,
    }
}

/// Parsing options for different Unity versions
#[derive(Debug, Clone)]
pub struct ParsingOptions {
    pub enable_type_tree: bool,
    pub use_big_ids: bool,
    pub supports_script_types: bool,
    pub supports_ref_types: bool,
    pub uses_extended_format: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_creation() {
        let processor = create_processor();
        assert!(!processor.has_file());
    }

    #[test]
    fn test_version_support() {
        assert!(is_version_supported(19));
        assert!(is_version_supported(5));
        assert!(!is_version_supported(100));
    }

    #[test]
    fn test_parsing_options() {
        let options = get_parsing_options(19);
        assert!(options.enable_type_tree);
        assert!(options.use_big_ids);
        assert!(options.supports_script_types);

        let old_options = get_parsing_options(10);
        assert!(!old_options.enable_type_tree);
        assert!(!old_options.use_big_ids);
    }

    #[test]
    fn test_supported_versions() {
        let versions = get_supported_versions();
        assert!(versions.contains(&19));
        assert!(versions.contains(&5));
        assert!(!versions.is_empty());
    }
}
