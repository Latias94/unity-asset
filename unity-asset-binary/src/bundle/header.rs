//! AssetBundle header parsing
//!
//! This module handles the parsing of AssetBundle headers,
//! supporting both legacy and UnityFS formats.

use crate::compression::{ArchiveFlags, CompressionType};
use crate::error::{BinaryError, Result};
use crate::reader::BinaryReader;
use serde::{Deserialize, Serialize};

/// AssetBundle header information
///
/// Contains metadata about the bundle including version, compression settings,
/// and structural information needed for parsing the bundle contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct BundleHeader {
    /// Bundle signature (e.g., "UnityFS", "UnityWeb", "UnityRaw")
    pub signature: String,
    /// Bundle format version
    pub version: u32,
    /// Unity version that created this bundle
    pub unity_version: String,
    /// Unity revision
    pub unity_revision: String,
    /// Total bundle size
    pub size: u64,
    /// Compressed blocks info size
    pub compressed_blocks_info_size: u32,
    /// Uncompressed blocks info size
    pub uncompressed_blocks_info_size: u32,
    /// Archive flags (compression type, block info location, etc.)
    pub flags: u32,
    /// Actual header size (recorded during parsing)
    pub actual_header_size: u64,
}


impl BundleHeader {
    /// Parse bundle header from binary data
    ///
    /// This method reads the bundle header from a binary reader,
    /// handling different bundle formats (UnityFS, UnityWeb, etc.).
    pub fn from_reader(reader: &mut BinaryReader) -> Result<Self> {
        let signature = reader.read_cstring()?;
        let version = reader.read_u32()?;
        let unity_version = reader.read_cstring()?;
        let unity_revision = reader.read_cstring()?;

        let mut header = Self {
            signature: signature.clone(),
            version,
            unity_version,
            unity_revision,
            size: 0,
            compressed_blocks_info_size: 0,
            uncompressed_blocks_info_size: 0,
            flags: 0,
            actual_header_size: 0,
        };

        // Read additional fields based on bundle format
        match signature.as_str() {
            "UnityFS" => {
                // Modern UnityFS format
                header.size = reader.read_i64()? as u64;
                header.compressed_blocks_info_size = reader.read_u32()?;
                header.uncompressed_blocks_info_size = reader.read_u32()?;
                header.flags = reader.read_u32()?;
            }
            "UnityWeb" | "UnityRaw" => {
                // Legacy formats
                header.size = reader.read_u32()? as u64;
                // Legacy formats don't have block info sizes or flags
                header.compressed_blocks_info_size = 0;
                header.uncompressed_blocks_info_size = 0;
                header.flags = 0;

                // Skip padding byte for some legacy versions
                if version < 6 {
                    reader.read_u8()?;
                }
            }
            _ => {
                return Err(BinaryError::unsupported(format!(
                    "Unknown bundle signature: {}",
                    signature
                )));
            }
        }

        // Record the actual header size
        header.actual_header_size = reader.position();

        Ok(header)
    }

    /// Get the compression type from flags
    pub fn compression_type(&self) -> Result<CompressionType> {
        CompressionType::from_flags(self.flags & ArchiveFlags::COMPRESSION_TYPE_MASK)
    }

    /// Check if block info is at the end of the file
    pub fn block_info_at_end(&self) -> bool {
        (self.flags & ArchiveFlags::BLOCK_INFO_AT_END) != 0
    }

    /// Check if this is a UnityFS format bundle
    pub fn is_unity_fs(&self) -> bool {
        self.signature == "UnityFS"
    }

    /// Check if this is a legacy format bundle
    pub fn is_legacy(&self) -> bool {
        matches!(self.signature.as_str(), "UnityWeb" | "UnityRaw")
    }

    /// Get the expected data offset after the header
    pub fn data_offset(&self) -> u64 {
        // This is typically calculated based on header size and block info location
        if self.block_info_at_end() {
            // Block info is at the end, data starts right after header
            self.header_size()
        } else {
            // Block info is at the beginning, data starts after block info
            self.header_size() + self.compressed_blocks_info_size as u64
        }
    }

    /// Calculate the size of the header itself
    pub fn header_size(&self) -> u64 {
        // Use the actual header size recorded during parsing
        // This is more accurate than calculating it
        if self.actual_header_size > 0 {
            self.actual_header_size
        } else {
            // Fallback to calculation if actual size not recorded
            let base_size = match self.signature.as_str() {
                "UnityFS" => {
                    // Signature + version + unity_version + unity_revision + size + compressed_size + uncompressed_size + flags
                    self.signature.len()
                        + 1
                        + 4
                        + self.unity_version.len()
                        + 1
                        + self.unity_revision.len()
                        + 1
                        + 8
                        + 4
                        + 4
                        + 4
                }
                "UnityWeb" | "UnityRaw" => {
                    // Signature + version + unity_version + unity_revision + size
                    self.signature.len()
                        + 1
                        + 4
                        + self.unity_version.len()
                        + 1
                        + self.unity_revision.len()
                        + 1
                        + 4
                }
                _ => 0,
            };

            // Add padding for alignment
            let aligned_size = (base_size + 15) & !15; // Align to 16 bytes
            aligned_size as u64
        }
    }

    /// Validate the header for consistency
    pub fn validate(&self) -> Result<()> {
        if self.signature.is_empty() {
            return Err(BinaryError::invalid_data("Empty bundle signature"));
        }

        if !matches!(self.signature.as_str(), "UnityFS" | "UnityWeb" | "UnityRaw") {
            return Err(BinaryError::unsupported(format!(
                "Unsupported bundle signature: {}",
                self.signature
            )));
        }

        if self.version == 0 {
            return Err(BinaryError::invalid_data("Invalid bundle version"));
        }

        if self.size == 0 {
            return Err(BinaryError::invalid_data("Invalid bundle size"));
        }

        // UnityFS specific validations
        if self.is_unity_fs() {
            if self.compressed_blocks_info_size == 0 && self.uncompressed_blocks_info_size == 0 {
                return Err(BinaryError::invalid_data("Invalid block info sizes"));
            }

            // Validate compression type
            self.compression_type()?;
        }

        Ok(())
    }

    /// Get bundle format information
    pub fn format_info(&self) -> BundleFormatInfo {
        BundleFormatInfo {
            signature: self.signature.clone(),
            version: self.version,
            is_compressed: self
                .compression_type()
                .map(|ct| ct != CompressionType::None)
                .unwrap_or(false),
            supports_streaming: self.is_unity_fs(),
            has_directory_info: self.is_unity_fs(),
        }
    }
}

/// Bundle format information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleFormatInfo {
    pub signature: String,
    pub version: u32,
    pub is_compressed: bool,
    pub supports_streaming: bool,
    pub has_directory_info: bool,
}

/// Bundle signature constants
pub mod signatures {
    pub const UNITY_FS: &str = "UnityFS";
    pub const UNITY_WEB: &str = "UnityWeb";
    pub const UNITY_RAW: &str = "UnityRaw";
}

/// Bundle version constants
pub mod versions {
    pub const UNITY_FS_MIN: u32 = 6;
    pub const UNITY_FS_CURRENT: u32 = 7;
    pub const UNITY_WEB_MIN: u32 = 3;
    pub const UNITY_RAW_MIN: u32 = 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundle_header_validation() {
        let mut header = BundleHeader::default();

        // Empty header should fail validation
        assert!(header.validate().is_err());

        // Set minimum required fields
        header.signature = "UnityFS".to_string();
        header.version = 6;
        header.size = 1000;
        header.compressed_blocks_info_size = 100;
        header.uncompressed_blocks_info_size = 200;

        // Should now pass validation
        assert!(header.validate().is_ok());
    }

    #[test]
    fn test_bundle_format_detection() {
        let header = BundleHeader {
            signature: "UnityFS".to_string(),
            version: 6,
            ..Default::default()
        };

        assert!(header.is_unity_fs());
        assert!(!header.is_legacy());

        let legacy_header = BundleHeader {
            signature: "UnityWeb".to_string(),
            version: 3,
            ..Default::default()
        };

        assert!(!legacy_header.is_unity_fs());
        assert!(legacy_header.is_legacy());
    }
}
