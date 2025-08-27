//! SerializedFile header parsing
//!
//! This module handles the parsing of Unity SerializedFile headers,
//! supporting different Unity versions and formats.

use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use serde::{Deserialize, Serialize};

/// Header of a Unity SerializedFile
/// 
/// Contains metadata about the serialized file including version information,
/// data layout, and endianness settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedFileHeader {
    /// Size of the metadata section
    pub metadata_size: u32,
    /// Total file size
    pub file_size: u32,
    /// File format version
    pub version: u32,
    /// Offset to the data section
    pub data_offset: u32,
    /// Endianness (0 = little, 1 = big)
    pub endian: u8,
    /// Reserved bytes
    pub reserved: [u8; 3],
}

impl SerializedFileHeader {
    /// Parse header from binary data (improved based on unity-rs)
    pub fn from_reader(reader: &mut BinaryReader) -> Result<Self> {
        let mut metadata_size = reader.read_u32()?;
        let mut file_size = reader.read_u32()?;
        let version = reader.read_u32()?;
        let mut data_offset = reader.read_u32()?;

        let endian;
        let mut reserved = [0u8; 3];

        // Handle different Unity versions (based on unity-rs logic)
        if version >= 9 {
            endian = reader.read_u8()?;
            let reserved_bytes = reader.read_bytes(3)?;
            reserved.copy_from_slice(&reserved_bytes);
        } else {
            // For older versions, endian is at the end of metadata
            let current_pos = reader.position();
            reader.set_position((file_size - metadata_size) as u64)?;
            endian = reader.read_u8()?;
            reader.set_position(current_pos)?;
        }

        // Handle version 22+ format changes
        if version >= 22 {
            metadata_size = reader.read_u32()?;
            file_size = reader.read_i64()? as u32;
            data_offset = reader.read_i64()? as u32;
            reader.read_i64()?; // Skip unknown field
        }

        Ok(Self {
            metadata_size,
            file_size,
            version,
            data_offset,
            endian,
            reserved,
        })
    }

    /// Get the byte order from the endian flag
    pub fn byte_order(&self) -> ByteOrder {
        if self.endian == 0 {
            ByteOrder::Little
        } else {
            ByteOrder::Big
        }
    }

    /// Check if this is a valid Unity file header
    pub fn is_valid(&self) -> bool {
        // Basic sanity checks
        self.version > 0
            && self.version < 100
            && self.data_offset > 0
            && self.file_size > self.data_offset
    }

    /// Get header format information
    pub fn format_info(&self) -> HeaderFormatInfo {
        HeaderFormatInfo {
            version: self.version,
            is_big_endian: self.endian != 0,
            has_extended_format: self.version >= 22,
            supports_large_files: self.version >= 22,
            metadata_size: self.metadata_size,
            data_offset: self.data_offset,
        }
    }

    /// Validate header consistency
    pub fn validate(&self) -> Result<()> {
        if !self.is_valid() {
            return Err(BinaryError::invalid_data("Invalid SerializedFile header"));
        }

        if self.metadata_size == 0 {
            return Err(BinaryError::invalid_data("Metadata size cannot be zero"));
        }

        if self.data_offset < self.metadata_size {
            return Err(BinaryError::invalid_data(
                "Data offset cannot be less than metadata size"
            ));
        }

        if self.file_size < self.data_offset {
            return Err(BinaryError::invalid_data(
                "File size cannot be less than data offset"
            ));
        }

        Ok(())
    }

    /// Get the size of the header itself
    pub fn header_size(&self) -> u32 {
        if self.version >= 22 {
            // Extended format: metadata_size + file_size + version + data_offset + endian + reserved + extended fields
            4 + 4 + 4 + 4 + 1 + 3 + 4 + 8 + 8 + 8 // 48 bytes
        } else if self.version >= 9 {
            // Standard format: metadata_size + file_size + version + data_offset + endian + reserved
            4 + 4 + 4 + 4 + 1 + 3 // 20 bytes
        } else {
            // Legacy format: metadata_size + file_size + version + data_offset (endian at end)
            4 + 4 + 4 + 4 // 16 bytes
        }
    }

    /// Check if this version supports TypeTrees
    pub fn supports_type_trees(&self) -> bool {
        self.version >= 7
    }

    /// Check if this version supports script types
    pub fn supports_script_types(&self) -> bool {
        self.version >= 11
    }

    /// Check if this version uses the new object format
    pub fn uses_new_object_format(&self) -> bool {
        self.version >= 14
    }
}

impl Default for SerializedFileHeader {
    fn default() -> Self {
        Self {
            metadata_size: 0,
            file_size: 0,
            version: 19, // Default to Unity 2019+ format
            data_offset: 0,
            endian: 0, // Little endian by default
            reserved: [0; 3],
        }
    }
}

/// Header format information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderFormatInfo {
    pub version: u32,
    pub is_big_endian: bool,
    pub has_extended_format: bool,
    pub supports_large_files: bool,
    pub metadata_size: u32,
    pub data_offset: u32,
}

/// Header validation result
#[derive(Debug, Clone)]
pub struct HeaderValidation {
    pub is_valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl HeaderValidation {
    pub fn new() -> Self {
        Self {
            is_valid: true,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn add_error(&mut self, error: String) {
        self.is_valid = false;
        self.errors.push(error);
    }

    pub fn add_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }
}

impl Default for HeaderValidation {
    fn default() -> Self {
        Self::new()
    }
}

/// Comprehensive header validation
pub fn validate_header(header: &SerializedFileHeader) -> HeaderValidation {
    let mut validation = HeaderValidation::new();

    // Basic validation
    if let Err(e) = header.validate() {
        validation.add_error(e.to_string());
        return validation;
    }

    // Version-specific warnings
    if header.version < 7 {
        validation.add_warning("Very old Unity version, limited feature support".to_string());
    }

    if header.version > 50 {
        validation.add_warning("Very new Unity version, may have compatibility issues".to_string());
    }

    // Endianness warnings
    if header.endian != 0 {
        validation.add_warning("Big-endian format detected, ensure proper handling".to_string());
    }

    // Size warnings
    if header.file_size > 1024 * 1024 * 1024 {
        validation.add_warning("Large file size (>1GB), may impact performance".to_string());
    }

    validation
}

/// Unity version constants for header validation
pub mod versions {
    pub const MIN_SUPPORTED: u32 = 5;
    pub const FIRST_WITH_TYPETREE: u32 = 7;
    pub const FIRST_WITH_ENDIAN_FLAG: u32 = 9;
    pub const FIRST_WITH_SCRIPT_TYPES: u32 = 11;
    pub const FIRST_WITH_NEW_OBJECTS: u32 = 14;
    pub const FIRST_WITH_EXTENDED_FORMAT: u32 = 22;
    pub const CURRENT_RECOMMENDED: u32 = 19;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_validation() {
        let mut header = SerializedFileHeader::default();
        header.version = 19;
        header.file_size = 1000;
        header.data_offset = 100;
        header.metadata_size = 50;

        assert!(header.is_valid());
        assert!(header.validate().is_ok());
    }

    #[test]
    fn test_byte_order() {
        let mut header = SerializedFileHeader::default();
        
        header.endian = 0;
        assert_eq!(header.byte_order(), ByteOrder::Little);
        
        header.endian = 1;
        assert_eq!(header.byte_order(), ByteOrder::Big);
    }

    #[test]
    fn test_version_features() {
        let mut header = SerializedFileHeader::default();
        
        header.version = 6;
        assert!(!header.supports_type_trees());
        
        header.version = 7;
        assert!(header.supports_type_trees());
        
        header.version = 11;
        assert!(header.supports_script_types());
        
        header.version = 22;
        assert!(header.uses_new_object_format());
    }
}
