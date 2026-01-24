//! AssetBundle header parsing
//!
//! This module handles the parsing of AssetBundle headers,
//! supporting both legacy and UnityFS formats.

use crate::compression::{ArchiveFlags, CompressionType};
use crate::error::{BinaryError, Result};
use crate::reader::BinaryReader;
use serde::{Deserialize, Serialize};

/// Parsed header fields for legacy Unity bundles (`UnityWeb` / `UnityRaw`).
///
/// UnityPy reference: `repo-ref/UnityPy/UnityPy/files/BundleFile.py::BundleFile.read_web_raw`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LegacyWebRawHeader {
    /// Optional hash (version >= 4): 16 bytes.
    pub hash: Option<Vec<u8>>,
    /// Optional CRC (version >= 4).
    pub crc: Option<u32>,

    pub minimum_streamed_bytes: u32,
    /// Absolute offset to the start of the (compressed) directory+file-content blob.
    pub header_size: u32,
    pub number_of_levels_to_download_before_streaming: u32,
    pub level_count: i32,

    /// Size of the (compressed) directory+file-content blob.
    pub compressed_size: u32,
    /// Size of the (uncompressed) directory+file-content blob.
    pub uncompressed_size: u32,

    /// Complete file size (version >= 2).
    pub complete_file_size: Option<u32>,
    /// Directory info header size (version >= 3).
    pub file_info_header_size: Option<u32>,
}

/// AssetBundle header information
///
/// Contains metadata about the bundle including version, compression settings,
/// and structural information needed for parsing the bundle contents.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    /// Legacy header fields (`UnityWeb` / `UnityRaw`), if applicable.
    pub legacy_web_raw: Option<LegacyWebRawHeader>,
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
            legacy_web_raw: None,
        };

        // Read additional fields based on bundle format
        match signature.as_str() {
            "UnityFS" => {
                // Modern UnityFS format
                let size = reader.read_i64()?;
                if size < 0 {
                    return Err(BinaryError::invalid_data(format!(
                        "Negative bundle size in header: {}",
                        size
                    )));
                }
                header.size = size as u64;
                header.compressed_blocks_info_size = reader.read_u32()?;
                header.uncompressed_blocks_info_size = reader.read_u32()?;
                header.flags = reader.read_u32()?;
            }
            "UnityWeb" | "UnityRaw" => {
                // Legacy formats: parse header fields as UnityPy does in `read_web_raw`.
                // Note: UnityPy reads the hash/crc when version >= 4, but its `save_web_raw`
                // only supports version <= 3. We still parse newer headers for read parity.
                let mut legacy = LegacyWebRawHeader::default();

                if version >= 4 {
                    let hash = reader.read_bytes(16)?;
                    if hash.len() != 16 {
                        return Err(BinaryError::invalid_data(format!(
                            "Legacy bundle hash length mismatch: expected 16, got {}",
                            hash.len()
                        )));
                    }
                    legacy.hash = Some(hash);
                    legacy.crc = Some(reader.read_u32()?);
                }

                legacy.minimum_streamed_bytes = reader.read_u32()?;
                legacy.header_size = reader.read_u32()?;
                legacy.number_of_levels_to_download_before_streaming = reader.read_u32()?;
                legacy.level_count = reader.read_i32()?;

                if legacy.level_count < 1 {
                    return Err(BinaryError::invalid_data(format!(
                        "Invalid legacy bundle levelCount: {}",
                        legacy.level_count
                    )));
                }

                // Skip all but the last level's size pairs.
                if legacy.level_count > 1 {
                    let skip = 8u64
                        .checked_mul((legacy.level_count as u64).saturating_sub(1))
                        .ok_or_else(|| {
                            BinaryError::invalid_data("Legacy levelCount skip overflow")
                        })?;
                    reader.skip_bytes(skip as usize)?;
                }

                legacy.compressed_size = reader.read_u32()?;
                legacy.uncompressed_size = reader.read_u32()?;

                if version >= 2 {
                    legacy.complete_file_size = Some(reader.read_u32()?);
                }
                if version >= 3 {
                    legacy.file_info_header_size = Some(reader.read_u32()?);
                }

                header.size = legacy
                    .complete_file_size
                    .unwrap_or(legacy.minimum_streamed_bytes) as u64;

                // Legacy formats don't have block info sizes or flags.
                header.compressed_blocks_info_size = 0;
                header.uncompressed_blocks_info_size = 0;
                header.flags = 0;

                // For legacy bundles, the "header size" we care about is the absolute offset to the data blob.
                header.actual_header_size = legacy.header_size as u64;
                header.legacy_web_raw = Some(legacy);
            }
            _ => {
                return Err(BinaryError::unsupported(format!(
                    "Unknown bundle signature: {}",
                    signature
                )));
            }
        }

        // Record the actual header size (for UnityFS we record the reader position;
        // for legacy bundles this is already set to the `headerSize` field above).
        if !header.is_legacy() {
            header.actual_header_size = reader.position();
        }

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
        // Empty header should fail validation
        let empty = BundleHeader::default();
        assert!(empty.validate().is_err());

        // Minimum required fields should pass validation
        let header = BundleHeader {
            signature: "UnityFS".to_string(),
            version: 6,
            size: 1000,
            compressed_blocks_info_size: 100,
            uncompressed_blocks_info_size: 200,
            ..Default::default()
        };
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
