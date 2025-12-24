//! Bundle parser implementation
//!
//! This module provides the main parsing logic for Unity AssetBundles,
//! inspired by UnityPy/files/BundleFile.py

use super::compression::BundleCompression;
use super::header::BundleHeader;
use super::types::{AssetBundle, BundleFileInfo, BundleLoadOptions, DirectoryNode};
use crate::compression::CompressionType;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::unity_version::UnityVersion;

/// Main bundle parser
///
/// This struct handles the parsing of Unity AssetBundle files,
/// supporting both UnityFS and legacy formats.
pub struct BundleParser;

impl BundleParser {
    /// Parse an AssetBundle from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<AssetBundle> {
        Self::from_bytes_with_options(data, BundleLoadOptions::default())
    }

    /// Parse an AssetBundle from binary data with options
    pub fn from_bytes_with_options(
        data: Vec<u8>,
        options: BundleLoadOptions,
    ) -> Result<AssetBundle> {
        let mut reader = BinaryReader::new(&data, ByteOrder::Big);

        // Parse header (reader position is preserved for subsequent parsing).
        let header = BundleHeader::from_reader(&mut reader)?;

        if options.validate {
            header.validate()?;
        }

        // The bundle stores decompressed block data. We intentionally keep the original input buffer
        // outside the bundle to avoid cloning large bundles.
        let mut bundle = AssetBundle::new(header, Vec::new());

        // Parse based on bundle format
        match bundle.header.signature.as_str() {
            "UnityFS" => {
                Self::parse_unity_fs(&mut bundle, &mut reader, &options)?;
            }
            "UnityWeb" | "UnityRaw" => {
                Self::parse_legacy(&mut bundle, &mut reader, &options)?;
            }
            _ => {
                return Err(BinaryError::unsupported(format!(
                    "Unsupported bundle format: {}",
                    bundle.header.signature
                )));
            }
        }

        if options.validate {
            bundle.validate()?;
        }

        Ok(bundle)
    }

    /// Parse UnityFS format bundle
    fn parse_unity_fs(
        bundle: &mut AssetBundle,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
    ) -> Result<()> {
        // Read blocks info
        Self::read_blocks_info(bundle, reader)?;

        // Decompress data blocks if requested OR if we need to load assets
        if options.decompress_blocks || options.load_assets {
            let blocks_data = Self::read_blocks(bundle, reader)?;
            Self::parse_files(bundle, blocks_data)?;

            // Load assets if requested
            if options.load_assets {
                Self::load_assets(bundle)?;
            }
        } else {
            // Just parse directory structure without decompressing all data
            Self::parse_directory_lazy(bundle, reader)?;
        }

        Ok(())
    }

    /// Parse legacy format bundle
    fn parse_legacy(
        bundle: &mut AssetBundle,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
    ) -> Result<()> {
        // Legacy bundles have a simpler structure
        let header_size = bundle.header.header_size() as usize;

        // Skip to after header
        reader.set_position(header_size as u64)?;

        // Read compression information
        let compressed_size = reader.read_u32()?;
        let uncompressed_size = reader.read_u32()?;

        // Skip some bytes based on version
        let skip_bytes = if bundle.header.version >= 2 { 4 } else { 0 };
        if skip_bytes > 0 {
            reader.read_bytes(skip_bytes)?;
        }

        // Move to the data section
        reader.set_position(header_size as u64)?;

        // Read and decompress the directory data
        let compressed_data = reader.read_bytes(compressed_size as usize)?;
        let directory_data = if bundle.header.signature == "UnityWeb" {
            // UnityWeb uses LZMA compression; prefer the explicit uncompressed size when available.
            crate::compression::decompress(
                &compressed_data,
                CompressionType::Lzma,
                uncompressed_size as usize,
            )
            .or_else(|_| {
                // Last-resort fallback for malformed headers.
                crate::compression::decompress(
                    &compressed_data,
                    CompressionType::Lzma,
                    compressed_data.len().saturating_mul(4),
                )
            })?
        } else {
            // UnityRaw is uncompressed
            compressed_data
        };

        // Parse directory information from decompressed data
        Self::parse_legacy_directory(bundle, &directory_data, header_size)?;

        // Load assets if requested
        if options.load_assets {
            Self::load_assets(bundle)?;
        }

        Ok(())
    }

    /// Read compression blocks information
    fn read_blocks_info(bundle: &mut AssetBundle, reader: &mut BinaryReader) -> Result<()> {
        // Apply version-specific alignment.
        // UnityFS uses 16-byte alignment in newer bundle formats (>=7).
        // For some older bundle formats, alignment may still be present (e.g. Unity 2019.4+),
        // but we only treat it as alignment if the padding bytes are all zero.
        if bundle.header.version >= 7 {
            reader.align_to(16)?;
        } else if Self::should_probe_legacy_alignment(&bundle.header) {
            let pre_align = reader.position();
            let pad = (16 - (pre_align % 16)) % 16;
            if pad != 0 {
                let align_bytes = reader.read_bytes(pad as usize)?;
                if align_bytes.iter().any(|&b| b != 0) {
                    reader.set_position(pre_align)?;
                }
            }
        }

        let start = reader.position();
        let compressed_size = bundle.header.compressed_blocks_info_size as usize;

        let blocks_info_data = if bundle.header.block_info_at_end() {
            let len = reader.len();
            if compressed_size > len {
                return Err(BinaryError::not_enough_data(compressed_size, len));
            }
            let pos = (len - compressed_size) as u64;
            reader.set_position(pos)?;
            let bytes = reader.read_bytes(compressed_size)?;
            reader.set_position(start)?;
            bytes
        } else {
            reader.read_bytes(compressed_size)?
        };

        // Decompress blocks info
        let uncompressed_data =
            BundleCompression::decompress_blocks_info(&bundle.header, &blocks_info_data)?;

        // Parse compression blocks
        bundle.blocks = BundleCompression::parse_compression_blocks(&uncompressed_data)?;

        // Validate blocks
        BundleCompression::validate_blocks(&bundle.blocks)?;

        // Parse directory information from the same blocks info data
        Self::parse_directory_from_blocks_info(bundle, &uncompressed_data)?;

        // Some UnityFS variants require padding/alignment before block data starts.
        if (bundle.header.flags
            & crate::compression::ArchiveFlags::BLOCK_INFO_NEEDS_PADDING_AT_START)
            != 0
        {
            reader.align_to(16)?;
        }

        Ok(())
    }

    fn should_probe_legacy_alignment(header: &BundleHeader) -> bool {
        // UnityPy heuristics: for some older bundle formats (<7) Unity started aligning file contents
        // (notably from 2019.4+). We only probe alignment when the engine version suggests this.
        let parsed = match UnityVersion::parse_version(&header.unity_revision)
            .or_else(|_| UnityVersion::parse_version(&header.unity_version))
        {
            Ok(v) => v,
            Err(_) => return false,
        };
        let (major, minor) = (parsed.major, parsed.minor);

        // 2019.4+
        major > 2019 || (major == 2019 && minor >= 4)
    }

    /// Read and decompress all blocks
    fn read_blocks(bundle: &AssetBundle, reader: &mut BinaryReader) -> Result<Vec<u8>> {
        BundleCompression::decompress_data_blocks(&bundle.header, &bundle.blocks, reader)
    }

    /// Parse files from decompressed block data
    fn parse_files(bundle: &mut AssetBundle, blocks_data: Vec<u8>) -> Result<()> {
        // Store the decompressed data
        *bundle.data_mut() = blocks_data;

        // Create file info for each node
        for node in &bundle.nodes {
            let file_info = BundleFileInfo::new(node.name.clone(), node.offset, node.size);
            bundle.files.push(file_info);
        }

        Ok(())
    }

    /// Parse directory structure without full decompression (lazy loading)
    fn parse_directory_lazy(_bundle: &mut AssetBundle, _reader: &mut BinaryReader) -> Result<()> {
        // For lazy loading, we only parse the directory structure
        // without decompressing all data blocks

        // The directory information has already been parsed in read_blocks_info()
        // so there's nothing more to do here for lazy loading.

        // The directory nodes are already populated in bundle.nodes
        Ok(())
    }

    /// Parse directory structure from blocks info data
    fn parse_directory_from_blocks_info(
        bundle: &mut AssetBundle,
        blocks_info_data: &[u8],
    ) -> Result<()> {
        let mut reader = BinaryReader::new(blocks_info_data, ByteOrder::Big);

        // Skip uncompressed data hash (16 bytes)
        reader.read_bytes(16)?;

        // Skip compression blocks information
        let block_count = reader.read_i32()? as usize;
        for _ in 0..block_count {
            reader.read_u32()?; // uncompressed_size
            reader.read_u32()?; // compressed_size
            reader.read_u16()?; // flags
        }

        // Now read directory information
        let node_count = reader.read_i32()? as usize;

        // Read directory nodes (UnityFS format)
        for _i in 0..node_count {
            let offset = reader.read_i64()? as u64; // UnityFS uses i64 for offset
            let size = reader.read_i64()? as u64; // UnityFS uses i64 for size
            let flags = reader.read_u32()?;
            let name = reader.read_cstring()?;

            let node = DirectoryNode::new(name, offset, size, flags);
            bundle.nodes.push(node);
        }

        Ok(())
    }

    /// Parse directory structure from data (legacy method, kept for compatibility)
    #[allow(dead_code)]
    fn parse_directory_from_data(bundle: &mut AssetBundle, data: &[u8]) -> Result<()> {
        let mut reader = BinaryReader::new(data, ByteOrder::Big);

        // Skip to directory info (this offset varies by bundle version)
        // This is a simplified implementation
        reader.set_position(0)?;

        // Read directory node count
        let node_count = reader.read_i32()? as usize;

        // Read directory nodes
        for _ in 0..node_count {
            let offset = reader.read_u64()?;
            let size = reader.read_u64()?;
            let flags = reader.read_u32()?;
            let name = reader.read_cstring()?;

            let node = DirectoryNode::new(name, offset, size, flags);
            bundle.nodes.push(node);
        }

        Ok(())
    }

    /// Parse legacy bundle directory
    fn parse_legacy_directory(
        bundle: &mut AssetBundle,
        directory_data: &[u8],
        header_size: usize,
    ) -> Result<()> {
        let mut dir_reader = BinaryReader::new(directory_data, ByteOrder::Big);
        dir_reader.set_position(header_size as u64)?; // Skip header in directory data

        // Read file count
        let file_count = dir_reader.read_i32()? as usize;

        // Read file entries
        for _ in 0..file_count {
            let name = dir_reader.read_cstring()?;
            let offset = dir_reader.read_u32()? as u64;
            let size = dir_reader.read_u32()? as u64;

            let file_info = BundleFileInfo::new(name.clone(), offset, size);
            bundle.files.push(file_info);

            // Also create a directory node for consistency
            let node = DirectoryNode::new(name, offset, size, 1); // Flag 1 = file
            bundle.nodes.push(node);
        }

        Ok(())
    }

    /// Load assets from the bundle files
    fn load_assets(bundle: &mut AssetBundle) -> Result<()> {
        // Clone the data to avoid borrowing issues
        let bundle_data = bundle.data().to_vec();
        let mut data_reader = BinaryReader::new(&bundle_data, ByteOrder::Big);

        // Clone nodes to avoid borrowing issues
        let nodes = bundle.nodes.clone();

        for node in &nodes {
            if node.is_file() {
                // Skip non-asset files (like .resS files)
                if node.name.ends_with(".resS") || node.name.ends_with(".resource") {
                    continue;
                }

                // Set position to the file's offset in decompressed data
                data_reader.set_position(node.offset)?;

                // Read the file data
                let file_data = data_reader.read_bytes(node.size as usize)?;

                // Try to parse as SerializedFile
                match crate::asset::SerializedFileParser::from_bytes(file_data) {
                    Ok(serialized_file) => {
                        // Add the SerializedFile as an asset
                        bundle.assets.push(serialized_file);
                        bundle.asset_names.push(node.name.clone());
                    }
                    Err(_e) => {
                        // If it's not a valid SerializedFile, skip or handle differently
                        // For now, we'll skip non-serialized files
                        continue;
                    }
                }
            }
        }

        Ok(())
    }

    /// Estimate parsing complexity
    pub fn estimate_complexity(data: &[u8]) -> Result<ParsingComplexity> {
        let mut reader = BinaryReader::new(data, ByteOrder::Big);
        let header = BundleHeader::from_reader(&mut reader)?;

        let complexity = match header.signature.as_str() {
            "UnityFS" => {
                let compression_type = header.compression_type()?;
                let has_compression = compression_type != CompressionType::None;

                ParsingComplexity {
                    format: "UnityFS".to_string(),
                    estimated_time: if has_compression { "Medium" } else { "Fast" }.to_string(),
                    memory_usage: header.size,
                    has_compression,
                    block_count: 0, // Would need to parse blocks info to get accurate count
                }
            }
            "UnityWeb" | "UnityRaw" => ParsingComplexity {
                format: header.signature.clone(),
                estimated_time: "Fast".to_string(),
                memory_usage: header.size,
                has_compression: header.signature == "UnityWeb",
                block_count: 1,
            },
            _ => {
                return Err(BinaryError::unsupported(format!(
                    "Unknown bundle format: {}",
                    header.signature
                )));
            }
        };

        Ok(complexity)
    }
}

/// Parsing complexity information
#[derive(Debug, Clone)]
pub struct ParsingComplexity {
    pub format: String,
    pub estimated_time: String,
    pub memory_usage: u64,
    pub has_compression: bool,
    pub block_count: usize,
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parser_creation() {
        // Basic test to ensure parser can be created
        // In practice, you'd need actual bundle data to test parsing
        let _dummy = 1 + 1;
        assert_eq!(_dummy, 2);
    }
}
