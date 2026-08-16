//! SerializedFile header parsing
//!
//! This module handles the parsing of Unity SerializedFile headers,
//! supporting different Unity versions and formats.

use super::format::{HeaderLayout, SerializedFileFormat};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryRead, BinaryReader, ByteOrder};
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
    pub file_size: u64,
    /// File format version
    pub version: u32,
    /// Offset to the data section
    pub data_offset: u64,
    /// Endianness (0 = little, 1 = big)
    pub endian: u8,
    /// Reserved bytes
    pub reserved: [u8; 3],
    /// Extended header unknown field (version >= 22).
    ///
    /// UnityPy stores this as `SerializedFile.unknown` and writes it back when saving.
    /// We keep it in the header to preserve round-trippability.
    pub unknown: i64,
}

impl SerializedFileHeader {
    /// Parses the fixed header using the shared format capability profile.
    pub fn from_reader(reader: &mut BinaryReader) -> Result<Self> {
        Self::from_input(reader)
    }

    pub(crate) fn from_input(input: &mut (impl BinaryRead + ?Sized)) -> Result<Self> {
        let mut metadata_size = input.read_u32()?;
        let mut file_size = u64::from(input.read_u32()?);
        let version = input.read_u32()?;
        let mut data_offset = u64::from(input.read_u32()?);
        let format = SerializedFileFormat::new(version)?;
        let mut reserved = [0u8; 3];
        let mut unknown = 0;
        let endian = match format.header_layout() {
            HeaderLayout::Legacy16 => {
                let current_position = input.position();
                let endian_position =
                    file_size
                        .checked_sub(u64::from(metadata_size))
                        .ok_or_else(|| {
                            BinaryError::invalid_data("SerializedFile metadata exceeds file size")
                        })?;
                input.set_position(endian_position)?;
                let endian = input.read_u8()?;
                input.set_position(current_position)?;
                endian
            }
            HeaderLayout::Standard20 | HeaderLayout::LargeFiles48 => {
                let endian = input.read_u8()?;
                let reserved_bytes = input.read_bytes(3)?;
                reserved.copy_from_slice(&reserved_bytes);
                if matches!(format.header_layout(), HeaderLayout::LargeFiles48) {
                    metadata_size = input.read_u32()?;
                    file_size = i64_to_u64_checked(input.read_i64()?, "file_size")?;
                    data_offset = i64_to_u64_checked(input.read_i64()?, "data_offset")?;
                    unknown = input.read_i64()?;
                }
                endian
            }
        };
        if endian > 1 {
            return Err(BinaryError::invalid_data(format!(
                "Invalid SerializedFile endian flag {endian}"
            )));
        }

        Ok(Self {
            metadata_size,
            file_size,
            version,
            data_offset,
            endian,
            reserved,
            unknown,
        })
    }

    /// Get the byte order from the endian flag
    pub const fn byte_order(&self) -> ByteOrder {
        if self.endian == 0 {
            ByteOrder::Little
        } else {
            ByteOrder::Big
        }
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
            unknown: 0,
        }
    }
}

fn i64_to_u64_checked(value: i64, name: &'static str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BinaryError::invalid_data(format!("Invalid {name}: negative value {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_view::DataView;
    use crate::random_access::ByteCursor;
    use crate::shared_bytes::SharedBytes;
    use unity_asset_core::AssetLoadBudget;

    #[test]
    fn large_header_parses_from_data_view_with_budgeted_cursor() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&22_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&48_u32.to_be_bytes());
        bytes.extend_from_slice(&48_i64.to_be_bytes());
        bytes.extend_from_slice(&48_i64.to_be_bytes());
        bytes.extend_from_slice(&7_i64.to_be_bytes());

        let view = DataView::from_shared(SharedBytes::from_vec(bytes));
        let mut budget = AssetLoadBudget::default();
        let header = {
            let mut input =
                ByteCursor::new(&view, ByteOrder::Big, &mut budget).expect("valid cursor");
            SerializedFileHeader::from_input(&mut input).expect("valid v22 header")
        };

        assert_eq!(header.metadata_size, 48);
        assert_eq!(header.file_size, 48);
        assert_eq!(header.data_offset, 48);
        assert_eq!(header.unknown, 7);
        assert_eq!(budget.usage().bytes, 48);
    }

    #[test]
    fn test_byte_order() {
        #[allow(clippy::field_reassign_with_default)]
        {
            let mut header = SerializedFileHeader::default();

            header.endian = 0;
            assert_eq!(header.byte_order(), ByteOrder::Little);

            header.endian = 1;
            assert_eq!(header.byte_order(), ByteOrder::Big);
        }
    }
}
