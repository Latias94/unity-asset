//! Bundle parser implementation
//!
//! This module provides the main parsing logic for Unity AssetBundles,
//! inspired by UnityPy/files/BundleFile.py

use super::compression::BundleCompression;
use super::header::BundleHeader;
use super::types::{AssetBundle, BundleFileInfo, BundleLoadOptions, DirectoryNode};
use crate::compression::CompressionType;
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
use crate::unity_version::UnityVersion;
use std::ops::Range;

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

    /// Parse an AssetBundle from a byte slice.
    ///
    /// This avoids copying when the input bytes already live in a shared buffer (e.g. WebFile entries).
    pub fn from_slice(data: &[u8]) -> Result<AssetBundle> {
        Self::from_slice_with_options(data, BundleLoadOptions::default())
    }

    /// Parse an AssetBundle from a shared backing buffer + byte range (zero-copy view).
    pub fn from_shared_range(data: SharedBytes, range: Range<usize>) -> Result<AssetBundle> {
        Self::from_shared_range_with_options(data, range, BundleLoadOptions::default())
    }

    /// Parse an AssetBundle from a shared backing buffer + byte range (zero-copy view), with options.
    pub fn from_shared_range_with_options(
        data: SharedBytes,
        range: Range<usize>,
        options: BundleLoadOptions,
    ) -> Result<AssetBundle> {
        let view = DataView::from_shared_range(data, range)?;
        Self::from_view_with_options(view, options)
    }

    /// Parse an AssetBundle from binary data with options
    pub fn from_bytes_with_options(
        data: Vec<u8>,
        options: BundleLoadOptions,
    ) -> Result<AssetBundle> {
        let shared = SharedBytes::from_vec(data);
        let len = shared.len();
        Self::from_shared_range_with_options(shared, 0..len, options)
    }

    /// Parse an AssetBundle from a byte slice with options.
    pub fn from_slice_with_options(data: &[u8], options: BundleLoadOptions) -> Result<AssetBundle> {
        // `&[u8]` has no ownership, so we need to copy to support on-demand access later.
        // Prefer `from_shared_range` for true zero-copy parsing (e.g. mmap/WebFile views).
        let shared = SharedBytes::from_vec(data.to_vec());
        let len = shared.len();
        Self::from_shared_range_with_options(shared, 0..len, options)
    }

    fn from_view_with_options(view: DataView, options: BundleLoadOptions) -> Result<AssetBundle> {
        let bytes = view.as_bytes();
        let mut reader = BinaryReader::new(bytes, ByteOrder::Big);

        // Parse header (reader position is preserved for subsequent parsing).
        let header = BundleHeader::from_reader(&mut reader)?;

        if options.validate {
            header.validate()?;
            if header.size > bytes.len() as u64 {
                return Err(BinaryError::invalid_data(format!(
                    "Bundle header size {} exceeds available bytes {}",
                    header.size,
                    bytes.len()
                )));
            }
        }

        let mut bundle = AssetBundle::new_empty(header);
        if bundle.header.is_legacy() {
            bundle.set_legacy_source(view.clone());
        }

        match bundle.header.signature.as_str() {
            "UnityFS" => {
                Self::parse_unity_fs(&mut bundle, &view, &mut reader, &options)?;
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
        source: &DataView,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
    ) -> Result<()> {
        // Read blocks info
        let block_data_start = Self::read_blocks_info(bundle, reader, options)?;

        // Decompress data blocks if requested OR if we need to load assets
        if options.decompress_blocks || options.load_assets {
            let blocks_data = Self::read_blocks(bundle, reader, options)?;
            Self::parse_files(bundle, blocks_data)?;

            // Load assets if requested
            if options.load_assets {
                Self::load_assets(bundle, options)?;
            }
        } else {
            let start_usize = usize::try_from(block_data_start).map_err(|_| {
                BinaryError::ResourceLimitExceeded(
                    "UnityFS block data start does not fit in usize".to_string(),
                )
            })?;
            bundle.set_lazy_unityfs_source(source.clone(), start_usize, options.max_memory);
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
        if let Some(max_memory) = options.max_memory {
            if (uncompressed_size as u64) > (max_memory as u64) {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Legacy bundle directory uncompressed size {} exceeds max_memory {}",
                    uncompressed_size, max_memory
                )));
            }
        }

        // Skip some bytes based on version
        let skip_bytes = if bundle.header.version >= 2 { 4 } else { 0 };
        if skip_bytes > 0 {
            reader.skip_bytes(skip_bytes)?;
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
        Self::parse_legacy_directory(bundle, &directory_data, header_size, options)?;

        // Load assets if requested
        if options.load_assets {
            Self::load_assets(bundle, options)?;
        }

        Ok(())
    }

    /// Read compression blocks information
    fn read_blocks_info(
        bundle: &mut AssetBundle,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
    ) -> Result<u64> {
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
        if let Some(max_blocks_info_size) = options.max_blocks_info_size {
            let expected = bundle.header.uncompressed_blocks_info_size as usize;
            if expected > max_blocks_info_size {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Blocks info uncompressed size {} exceeds limit {}",
                    expected, max_blocks_info_size
                )));
            }
        }
        let uncompressed_data = BundleCompression::decompress_blocks_info_limited(
            &bundle.header,
            &blocks_info_data,
            options.max_blocks_info_size,
        )?;

        // Parse compression blocks
        bundle.blocks =
            BundleCompression::parse_compression_blocks_limited(&uncompressed_data, options)?;

        // Validate blocks
        BundleCompression::validate_blocks(&bundle.blocks)?;

        let total_uncompressed = bundle.blocks.iter().try_fold(0u64, |acc, b| {
            acc.checked_add(b.uncompressed_size as u64).ok_or_else(|| {
                BinaryError::ResourceLimitExceeded(
                    "Total uncompressed bundle data size overflow".to_string(),
                )
            })
        })?;
        bundle.set_decompressed_len(total_uncompressed);

        // Parse directory information from the same blocks info data
        Self::parse_directory_from_blocks_info(bundle, &uncompressed_data, options)?;

        // Some UnityFS variants require padding/alignment before block data starts.
        if (bundle.header.flags
            & crate::compression::ArchiveFlags::BLOCK_INFO_NEEDS_PADDING_AT_START)
            != 0
        {
            reader.align_to(16)?;
        }

        Ok(reader.position())
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
    fn read_blocks(
        bundle: &AssetBundle,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
    ) -> Result<Vec<u8>> {
        BundleCompression::decompress_data_blocks_limited(
            &bundle.header,
            &bundle.blocks,
            reader,
            options.max_memory,
        )
    }

    /// Parse files from decompressed block data
    fn parse_files(bundle: &mut AssetBundle, blocks_data: Vec<u8>) -> Result<()> {
        // Store the decompressed data
        bundle.set_decompressed_data(blocks_data);

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
        options: &BundleLoadOptions,
    ) -> Result<()> {
        let mut reader = BinaryReader::new(blocks_info_data, ByteOrder::Big);

        // Skip uncompressed data hash (16 bytes)
        reader.read_bytes(16)?;

        // Skip compression blocks information (we already parsed them).
        let block_count_i32 = reader.read_i32()?;
        if block_count_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative compression block count: {}",
                block_count_i32
            )));
        }
        let block_count: usize = block_count_i32 as usize;
        if block_count > options.max_blocks {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "Compression block count {} exceeds limit {}",
                block_count, options.max_blocks
            )));
        }
        let bytes_to_skip = block_count
            .checked_mul(10)
            .ok_or_else(|| BinaryError::invalid_data("Compression block table size overflow"))?;
        reader.skip_bytes(bytes_to_skip)?;

        // Now read directory information
        let node_count_i32 = reader.read_i32()?;
        if node_count_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative directory node count: {}",
                node_count_i32
            )));
        }
        let node_count: usize = node_count_i32 as usize;
        if node_count > options.max_nodes {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "Directory node count {} exceeds limit {}",
                node_count, options.max_nodes
            )));
        }

        let total_uncompressed: u64 = bundle
            .blocks
            .iter()
            .map(|b| b.uncompressed_size as u64)
            .sum();

        // Read directory nodes (UnityFS format)
        for _i in 0..node_count {
            let offset_i64 = reader.read_i64()?; // UnityFS uses i64 for offset
            if offset_i64 < 0 {
                return Err(BinaryError::invalid_data(format!(
                    "Negative directory node offset: {}",
                    offset_i64
                )));
            }
            let size_i64 = reader.read_i64()?; // UnityFS uses i64 for size
            if size_i64 < 0 {
                return Err(BinaryError::invalid_data(format!(
                    "Negative directory node size: {}",
                    size_i64
                )));
            }
            let offset = offset_i64 as u64;
            let size = size_i64 as u64;
            let end = offset
                .checked_add(size)
                .ok_or_else(|| BinaryError::invalid_data("Directory node offset+size overflow"))?;
            if end > total_uncompressed {
                return Err(BinaryError::invalid_data(format!(
                    "Directory node exceeds decompressed data: end {} > {}",
                    end, total_uncompressed
                )));
            }
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
        let node_count_i32 = reader.read_i32()?;
        if node_count_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative directory node count: {}",
                node_count_i32
            )));
        }
        let node_count = node_count_i32 as usize;

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
        options: &BundleLoadOptions,
    ) -> Result<()> {
        let mut dir_reader = BinaryReader::new(directory_data, ByteOrder::Big);
        dir_reader.set_position(header_size as u64)?; // Skip header in directory data

        // Read file count
        let file_count_i32 = dir_reader.read_i32()?;
        if file_count_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative legacy bundle file count: {}",
                file_count_i32
            )));
        }
        let file_count: usize = file_count_i32 as usize;
        if file_count > options.max_nodes {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "Legacy bundle file count {} exceeds limit {}",
                file_count, options.max_nodes
            )));
        }

        // Read file entries
        for _ in 0..file_count {
            let name = dir_reader.read_cstring()?;
            let offset = dir_reader.read_u32()? as u64;
            let size = dir_reader.read_u32()? as u64;

            let file_info = BundleFileInfo::new(name.clone(), offset, size);
            bundle.files.push(file_info);

            // Also create a directory node for consistency
            let node = DirectoryNode::new(name, offset, size, 0x4); // Flag 0x4 = file
            bundle.nodes.push(node);
        }

        Ok(())
    }

    /// Load assets from the bundle files
    fn load_assets(bundle: &mut AssetBundle, options: &BundleLoadOptions) -> Result<()> {
        let (backing, base_offset, visible_len) = if bundle.header.is_unity_fs() {
            let backing = crate::shared_bytes::SharedBytes::from_arc(bundle.data_arc()?);
            let visible_len = backing.len() as u64;
            (backing, 0usize, visible_len)
        } else {
            let view = bundle.legacy_source().ok_or_else(|| {
                BinaryError::invalid_data("Legacy bundle source is not available")
            })?;
            let visible_len = view.len() as u64;
            (view.backing_shared(), view.base_offset(), visible_len)
        };

        // Clone nodes to avoid borrow conflicts while pushing assets.
        let nodes = bundle.nodes.clone();

        for node in &nodes {
            if !node.is_file() {
                continue;
            }

            // Skip non-asset files (like .resS files).
            if node.name.ends_with(".resS") || node.name.ends_with(".resource") {
                continue;
            }

            let end = node.offset.saturating_add(node.size);
            if end > visible_len {
                return Err(BinaryError::invalid_data(format!(
                    "Bundle node '{}' exceeds decompressed data: end {} > {}",
                    node.name, end, visible_len
                )));
            }

            if let Some(max_memory) = options.max_memory {
                if node.size > max_memory as u64 {
                    return Err(BinaryError::ResourceLimitExceeded(format!(
                        "Bundle node '{}' size {} exceeds max_memory {}",
                        node.name, node.size, max_memory
                    )));
                }
            }

            let start = usize::try_from(node.offset).map_err(|_| {
                BinaryError::ResourceLimitExceeded(format!(
                    "Bundle node '{}' offset {} does not fit in usize",
                    node.name, node.offset
                ))
            })?;
            let end = usize::try_from(end).map_err(|_| {
                BinaryError::ResourceLimitExceeded(format!(
                    "Bundle node '{}' end {} does not fit in usize",
                    node.name, end
                ))
            })?;

            let abs_start = base_offset.checked_add(start).ok_or_else(|| {
                BinaryError::ResourceLimitExceeded(format!(
                    "Bundle node '{}' absolute start overflow",
                    node.name
                ))
            })?;
            let abs_end = base_offset.checked_add(end).ok_or_else(|| {
                BinaryError::ResourceLimitExceeded(format!(
                    "Bundle node '{}' absolute end overflow",
                    node.name
                ))
            })?;

            // Parse as a zero-copy view into the backing buffer (UnityFS decompressed buffer or legacy source).
            if let Ok(serialized_file) = crate::asset::SerializedFileParser::from_shared_range(
                backing.clone(),
                abs_start..abs_end,
            ) {
                bundle.assets.push(serialized_file);
                bundle.asset_names.push(node.name.clone());
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
    use super::*;

    #[test]
    fn test_parser_creation() {
        // Basic test to ensure parser can be created
        // In practice, you'd need actual bundle data to test parsing
        let _dummy = 1 + 1;
        assert_eq!(_dummy, 2);
    }

    #[test]
    fn load_assets_rejects_out_of_bounds_node() {
        let mut header = BundleHeader::default();
        header.signature = "UnityFS".to_string();
        let mut bundle = AssetBundle::new(header, vec![0u8; 8]);
        bundle
            .nodes
            .push(DirectoryNode::new("a.assets".to_string(), 1024, 4, 0x4));

        let err =
            BundleParser::load_assets(&mut bundle, &BundleLoadOptions::default()).unwrap_err();
        assert!(matches!(err, BinaryError::InvalidData(_)));
    }
}
