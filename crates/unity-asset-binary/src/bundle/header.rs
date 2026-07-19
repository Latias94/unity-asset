//! AssetBundle header parsing
//!
//! This module handles the parsing of AssetBundle headers,
//! supporting both legacy and UnityFS formats.

use crate::compression::{ArchiveFlags, CompressionType};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryRead, BinaryReader};
use serde::{Deserialize, Serialize};

/// Physical header and payload layout selected by the wire signature and format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BundleLayoutKind {
    /// Block table plus independently compressed data blocks.
    FileStream,
    /// Pre-v6 UnityWeb/UnityRaw streaming header and combined payload.
    Legacy,
}

impl BundleLayoutKind {
    pub fn from_wire(signature: &str, version: u32) -> Result<Self> {
        match (signature, version) {
            ("UnityFS", _) | ("UnityWeb" | "UnityRaw", 6) => Ok(Self::FileStream),
            ("UnityWeb" | "UnityRaw", _) => Ok(Self::Legacy),
            _ => Err(BinaryError::unsupported(format!(
                "Unknown bundle signature: {signature}"
            ))),
        }
    }
}

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
    /// Extra byte stored after FileStream flags by UnityWeb/UnityRaw v6.
    #[serde(default)]
    pub file_stream_header_byte: Option<u8>,
}

impl BundleHeader {
    /// Parse bundle header from binary data
    ///
    /// This method reads the bundle header from a binary reader,
    /// handling different bundle formats (UnityFS, UnityWeb, etc.).
    pub fn from_reader(reader: &mut BinaryReader) -> Result<Self> {
        Self::from_input(reader)
    }

    pub(crate) fn from_input(input: &mut (impl BinaryRead + ?Sized)) -> Result<Self> {
        let signature = input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;
        let version = input.read_u32()?;
        let unity_version = input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;
        let unity_revision = input.read_cstring_limited(BinaryReader::DEFAULT_MAX_STRING_LEN)?;
        let layout = BundleLayoutKind::from_wire(&signature, version)?;

        let mut header = Self {
            signature,
            version,
            unity_version,
            unity_revision,
            size: 0,
            compressed_blocks_info_size: 0,
            uncompressed_blocks_info_size: 0,
            flags: 0,
            actual_header_size: 0,
            legacy_web_raw: None,
            file_stream_header_byte: None,
        };

        match layout {
            BundleLayoutKind::FileStream => {
                let size = input.read_i64()?;
                if size < 0 {
                    return Err(BinaryError::invalid_data(format!(
                        "Negative bundle size in header: {}",
                        size
                    )));
                }
                header.size = size as u64;
                header.compressed_blocks_info_size = input.read_u32()?;
                header.uncompressed_blocks_info_size = input.read_u32()?;
                header.flags = input.read_u32()?;
                if header.signature != "UnityFS" {
                    header.file_stream_header_byte = Some(input.read_u8()?);
                }
                header.actual_header_size = input.position();
            }
            BundleLayoutKind::Legacy => {
                let mut legacy = LegacyWebRawHeader::default();

                if version >= 4 {
                    let hash = input.read_bytes(16)?;
                    if hash.len() != 16 {
                        return Err(BinaryError::invalid_data(format!(
                            "Legacy bundle hash length mismatch: expected 16, got {}",
                            hash.len()
                        )));
                    }
                    legacy.hash = Some(hash);
                    legacy.crc = Some(input.read_u32()?);
                }

                legacy.minimum_streamed_bytes = input.read_u32()?;
                legacy.header_size = input.read_u32()?;
                legacy.number_of_levels_to_download_before_streaming = input.read_u32()?;
                legacy.level_count = input.read_i32()?;

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
                    let next = input.position().checked_add(skip).ok_or_else(|| {
                        BinaryError::invalid_data("Legacy levelCount position overflow")
                    })?;
                    input.set_position(next)?;
                }

                legacy.compressed_size = input.read_u32()?;
                legacy.uncompressed_size = input.read_u32()?;

                if version >= 2 {
                    legacy.complete_file_size = Some(input.read_u32()?);
                }
                if version >= 3 {
                    legacy.file_info_header_size = Some(input.read_u32()?);
                }

                if u64::from(legacy.header_size) < input.position() {
                    return Err(BinaryError::invalid_data(format!(
                        "Legacy header_size {} precedes parsed header end {}",
                        legacy.header_size,
                        input.position()
                    )));
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

    /// Returns the physical layout implied by the signature and version.
    pub fn layout_kind(&self) -> Result<BundleLayoutKind> {
        BundleLayoutKind::from_wire(&self.signature, self.version)
    }

    /// Check if this bundle uses the block-based FileStream layout.
    pub fn is_file_stream(&self) -> bool {
        matches!(self.layout_kind(), Ok(BundleLayoutKind::FileStream))
    }

    /// Check if this is a legacy format bundle
    pub fn is_legacy(&self) -> bool {
        matches!(self.layout_kind(), Ok(BundleLayoutKind::Legacy))
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
        if self.actual_header_size > 0 {
            return self.actual_header_size;
        }
        if let Some(legacy) = &self.legacy_web_raw {
            return u64::from(legacy.header_size);
        }

        let common = self
            .signature
            .len()
            .saturating_add(1 + 4)
            .saturating_add(self.unity_version.len() + 1)
            .saturating_add(self.unity_revision.len() + 1);
        let file_stream = 8 + 4 + 4 + 4 + usize::from(self.signature != "UnityFS");
        u64::try_from(common.saturating_add(file_stream)).unwrap_or(u64::MAX)
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

        if self.layout_kind()? == BundleLayoutKind::FileStream {
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
        let layout = self.layout_kind().ok();
        let is_compressed = match layout {
            Some(BundleLayoutKind::FileStream) => self
                .compression_type()
                .map(|compression| compression != CompressionType::None)
                .unwrap_or(false),
            Some(BundleLayoutKind::Legacy) => self.signature == "UnityWeb",
            None => false,
        };
        BundleFormatInfo {
            signature: self.signature.clone(),
            version: self.version,
            is_compressed,
            supports_streaming: layout == Some(BundleLayoutKind::FileStream),
            has_directory_info: layout == Some(BundleLayoutKind::FileStream),
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

        let web_v6 = BundleHeader {
            signature: "UnityWeb".to_string(),
            version: 6,
            ..Default::default()
        };
        assert!(!web_v6.is_unity_fs());
        assert!(web_v6.is_file_stream());
        assert!(!web_v6.is_legacy());

        let legacy_info = legacy_header.format_info();
        assert!(legacy_info.is_compressed);
        assert!(!legacy_info.has_directory_info);
        let web_v6_info = web_v6.format_info();
        assert!(!web_v6_info.is_compressed);
        assert!(web_v6_info.has_directory_info);
    }
}
