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
use std::mem::size_of;
use std::ops::Range;
use unity_asset_core::AssetLoadBudget;

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

    pub fn from_bytes_with_budget(
        data: Vec<u8>,
        budget: &mut AssetLoadBudget,
    ) -> Result<AssetBundle> {
        Self::from_bytes_with_options_and_budget(data, BundleLoadOptions::default(), budget)
    }

    /// Parse an AssetBundle from a byte slice.
    ///
    /// This avoids copying when the input bytes already live in a shared buffer (e.g. WebFile entries).
    pub fn from_slice(data: &[u8]) -> Result<AssetBundle> {
        Self::from_slice_with_options(data, BundleLoadOptions::default())
    }

    pub fn from_slice_with_budget(
        data: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<AssetBundle> {
        Self::from_slice_with_options_and_budget(data, BundleLoadOptions::default(), budget)
    }

    /// Parse an AssetBundle from a shared backing buffer + byte range (zero-copy view).
    pub fn from_shared_range(data: SharedBytes, range: Range<usize>) -> Result<AssetBundle> {
        Self::from_shared_range_with_options(data, range, BundleLoadOptions::default())
    }

    pub fn from_shared_range_with_budget(
        data: SharedBytes,
        range: Range<usize>,
        budget: &mut AssetLoadBudget,
    ) -> Result<AssetBundle> {
        Self::from_shared_range_with_options_and_budget(
            data,
            range,
            BundleLoadOptions::default(),
            budget,
        )
    }

    /// Parse an AssetBundle from a shared backing buffer + byte range (zero-copy view), with options.
    pub fn from_shared_range_with_options(
        data: SharedBytes,
        range: Range<usize>,
        options: BundleLoadOptions,
    ) -> Result<AssetBundle> {
        let mut budget = AssetLoadBudget::default();
        Self::from_shared_range_with_options_and_budget(data, range, options, &mut budget)
    }

    pub fn from_shared_range_with_options_and_budget(
        data: SharedBytes,
        range: Range<usize>,
        options: BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<AssetBundle> {
        let view = DataView::from_shared_range(data, range)?;
        Self::from_view_with_options_and_budget(view, options, budget)
    }

    /// Parse an AssetBundle from binary data with options
    pub fn from_bytes_with_options(
        data: Vec<u8>,
        options: BundleLoadOptions,
    ) -> Result<AssetBundle> {
        let mut budget = AssetLoadBudget::default();
        Self::from_bytes_with_options_and_budget(data, options, &mut budget)
    }

    pub fn from_bytes_with_options_and_budget(
        data: Vec<u8>,
        options: BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<AssetBundle> {
        let shared = SharedBytes::from_vec(data);
        let len = shared.len();
        Self::from_shared_range_with_options_and_budget(shared, 0..len, options, budget)
    }

    /// Parse an AssetBundle from a byte slice with options.
    pub fn from_slice_with_options(data: &[u8], options: BundleLoadOptions) -> Result<AssetBundle> {
        let mut budget = AssetLoadBudget::default();
        Self::from_slice_with_options_and_budget(data, options, &mut budget)
    }

    pub fn from_slice_with_options_and_budget(
        data: &[u8],
        options: BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<AssetBundle> {
        let source_len = u64::try_from(data.len())
            .map_err(|_| BinaryError::invalid_data("Bundle source length does not fit in u64"))?;
        if source_len > budget.remaining_bytes() {
            budget.consume_bytes(source_len)?;
            return Err(BinaryError::invalid_data(
                "Bundle byte budget accepted a request beyond its remaining allowance",
            ));
        }
        let mut owned = Vec::new();
        owned.try_reserve_exact(data.len()).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {} bytes for a bundle source: {error}",
                data.len()
            ))
        })?;
        owned.extend_from_slice(data);
        let shared = SharedBytes::from_vec(owned);
        let len = shared.len();
        Self::from_shared_range_with_options_and_budget(shared, 0..len, options, budget)
    }

    fn from_view_with_options_and_budget(
        view: DataView,
        options: BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<AssetBundle> {
        budget.consume_bytes(u64::try_from(view.len()).map_err(|_| {
            BinaryError::invalid_data("Bundle source length does not fit in u64")
        })?)?;
        let bytes = view.as_bytes();
        let mut reader = BinaryReader::new(bytes, ByteOrder::Big);

        // Parse header (reader position is preserved for subsequent parsing).
        let header = BundleHeader::from_reader(&mut reader)?;

        header.validate()?;
        Self::validate_declared_header_limits(&header, &options)?;
        if header.size
            > u64::try_from(bytes.len())
                .map_err(|_| BinaryError::invalid_data("Bundle length does not fit in u64"))?
        {
            return Err(BinaryError::invalid_data(format!(
                "Bundle header size {} exceeds available bytes {}",
                header.size,
                bytes.len()
            )));
        }

        let mut bundle = AssetBundle::new_empty(header);
        if bundle.header.is_legacy() {
            bundle.set_legacy_source(view.clone());
        }

        match bundle.header.signature.as_str() {
            "UnityFS" => {
                Self::parse_unity_fs(&mut bundle, &view, &mut reader, &options, budget)?;
            }
            "UnityWeb" | "UnityRaw" => {
                Self::parse_legacy(&mut bundle, &mut reader, &options, budget)?;
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

    fn validate_declared_header_limits(
        header: &BundleHeader,
        options: &BundleLoadOptions,
    ) -> Result<()> {
        if let Some(legacy) = &header.legacy_web_raw {
            if let Some(limit) = options.max_legacy_directory_compressed_size
                && u64::from(legacy.compressed_size) > limit as u64
            {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Legacy bundle directory compressed size {} exceeds limit {}",
                    legacy.compressed_size, limit
                )));
            }
            if let Some(limit) = options.max_memory
                && u64::from(legacy.uncompressed_size) > limit as u64
            {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Legacy bundle directory uncompressed size {} exceeds max_memory {}",
                    legacy.uncompressed_size, limit
                )));
            }
        }

        if header.is_unity_fs() {
            if let Some(limit) = options.max_compressed_blocks_info_size
                && u64::from(header.compressed_blocks_info_size) > limit as u64
            {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Blocks info compressed size {} exceeds limit {}",
                    header.compressed_blocks_info_size, limit
                )));
            }
            if let Some(limit) = options.max_blocks_info_size
                && u64::from(header.uncompressed_blocks_info_size) > limit as u64
            {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Blocks info uncompressed size {} exceeds limit {}",
                    header.uncompressed_blocks_info_size, limit
                )));
            }
        }

        Ok(())
    }

    /// Parse UnityFS format bundle
    fn parse_unity_fs(
        bundle: &mut AssetBundle,
        source: &DataView,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        // Read blocks info
        let block_data_start = Self::read_blocks_info(bundle, reader, options, budget)?;
        let block_data = Self::unityfs_block_data_view(bundle, source, block_data_start)?;

        // Decompress data blocks if requested OR if we need to load assets
        if options.decompress_blocks || options.load_assets {
            let mut block_reader = BinaryReader::new(block_data.as_bytes(), ByteOrder::Big);
            let blocks_data = Self::read_blocks(bundle, &mut block_reader, options, budget)?;
            Self::parse_files(bundle, blocks_data, budget)?;

            // Load assets if requested
            if options.load_assets {
                Self::load_assets(bundle, options, budget)?;
            }
        } else {
            bundle.set_lazy_unityfs_source(
                block_data,
                0,
                options.max_memory,
                options.max_unityfs_block_cache_memory,
                options.max_compressed_block_size,
            )?;
        }

        Ok(())
    }

    fn unityfs_block_data_view(
        bundle: &AssetBundle,
        source: &DataView,
        block_data_start: u64,
    ) -> Result<DataView> {
        let block_data_end = if bundle.header.block_info_at_end() {
            bundle
                .header
                .size
                .checked_sub(u64::from(bundle.header.compressed_blocks_info_size))
                .ok_or_else(|| {
                    BinaryError::invalid_data(
                        "UnityFS blocks-info size exceeds the declared bundle size",
                    )
                })?
        } else {
            bundle.header.size
        };
        if block_data_start > block_data_end {
            return Err(BinaryError::invalid_data(format!(
                "UnityFS block data starts at {block_data_start} after its physical end {block_data_end}"
            )));
        }

        let declared_compressed = bundle.blocks.iter().try_fold(0_u64, |total, block| {
            total
                .checked_add(u64::from(block.compressed_size))
                .ok_or_else(|| BinaryError::invalid_data("UnityFS compressed block total overflow"))
        })?;
        let available = block_data_end - block_data_start;
        if declared_compressed > available {
            return Err(BinaryError::invalid_data(format!(
                "UnityFS compressed block total {declared_compressed} exceeds physical payload range {available}"
            )));
        }

        let relative_start = usize::try_from(block_data_start).map_err(|_| {
            BinaryError::invalid_data("UnityFS block data start does not fit usize")
        })?;
        let relative_end = usize::try_from(block_data_end)
            .map_err(|_| BinaryError::invalid_data("UnityFS block data end does not fit usize"))?;
        let absolute_start = source
            .base_offset()
            .checked_add(relative_start)
            .ok_or_else(|| BinaryError::invalid_data("UnityFS block data start overflow"))?;
        let absolute_end = source
            .base_offset()
            .checked_add(relative_end)
            .ok_or_else(|| BinaryError::invalid_data("UnityFS block data end overflow"))?;
        DataView::from_shared_range(source.backing_shared(), absolute_start..absolute_end)
    }

    /// Parse legacy format bundle
    fn parse_legacy(
        bundle: &mut AssetBundle,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        let legacy = bundle.header.legacy_web_raw.as_ref().ok_or_else(|| {
            BinaryError::invalid_data("Legacy bundle header fields were not parsed")
        })?;

        let header_size = legacy.header_size as usize;
        let compressed_size = legacy.compressed_size;
        let uncompressed_size = legacy.uncompressed_size;
        budget.check_compressed_bytes(u64::from(compressed_size))?;

        // Seek to the (compressed) directory+file-content blob (UnityPy uses `reader.Position = headerSize`).
        reader.set_position(header_size as u64)?;

        // Read and decompress the directory data
        let compressed_data = reader.read_bytes(compressed_size as usize)?;
        let uncompressed_size = usize::try_from(uncompressed_size).map_err(|_| {
            BinaryError::invalid_data("Legacy bundle uncompressed size does not fit usize")
        })?;
        let directory_data = if bundle.header.signature == "UnityWeb" {
            crate::compression::decompress_lzma_size_stream_with_budget(
                &compressed_data,
                uncompressed_size,
                budget,
            )?
        } else {
            crate::compression::decompress_with_budget(
                &compressed_data,
                CompressionType::None,
                uncompressed_size,
                budget,
            )?
        };

        // Legacy bundles store directory entries + file content in the same blob.
        // Make that blob the active data source so node offsets can be interpreted relative to it.
        let directory_view = DataView::from_shared(SharedBytes::from_vec(directory_data));
        bundle.set_legacy_source(directory_view.clone());

        // Parse directory information from the uncompressed blob.
        Self::parse_legacy_directory(bundle, directory_view.as_bytes(), 0, options, budget)?;

        // Load assets if requested
        if options.load_assets {
            Self::load_assets(bundle, options, budget)?;
        }

        Ok(())
    }

    /// Read compression blocks information
    fn read_blocks_info(
        bundle: &mut AssetBundle,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<u64> {
        let compressed_size =
            usize::try_from(bundle.header.compressed_blocks_info_size).map_err(|_| {
                BinaryError::ResourceLimitExceeded(
                    "Blocks info compressed size does not fit in usize".to_string(),
                )
            })?;
        budget.check_compressed_bytes(u64::from(bundle.header.compressed_blocks_info_size))?;

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
        let blocks_info_data = if bundle.header.block_info_at_end() {
            let declared_len = usize::try_from(bundle.header.size).map_err(|_| {
                BinaryError::invalid_data("Bundle declared size does not fit in usize")
            })?;
            if compressed_size > declared_len {
                return Err(BinaryError::not_enough_data(compressed_size, declared_len));
            }
            let pos = u64::try_from(declared_len - compressed_size).map_err(|_| {
                BinaryError::invalid_data("Bundle block-info position does not fit in u64")
            })?;
            reader.set_position(pos)?;
            let bytes = reader.read_bytes(compressed_size)?;
            reader.set_position(start)?;
            bytes
        } else {
            reader.read_bytes(compressed_size)?
        };

        // Decompress blocks info
        let uncompressed_data = BundleCompression::decompress_blocks_info_limited_with_budget(
            &bundle.header,
            &blocks_info_data,
            options.max_blocks_info_size,
            budget,
        )?;

        // Parse compression blocks
        bundle.blocks = BundleCompression::parse_compression_blocks_limited_with_budget(
            &uncompressed_data,
            options,
            budget,
        )?;

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
        Self::parse_directory_from_blocks_info(bundle, &uncompressed_data, options, budget)?;

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
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<u8>> {
        if let Some(limit) = options.max_compressed_block_size {
            for block in &bundle.blocks {
                if (block.compressed_size as u64) > (limit as u64) {
                    return Err(BinaryError::ResourceLimitExceeded(format!(
                        "Block compressed size {} exceeds max_compressed_block_size {}",
                        block.compressed_size, limit
                    )));
                }
            }
        }
        BundleCompression::decompress_data_blocks_limited_with_budget(
            &bundle.header,
            &bundle.blocks,
            reader,
            options.max_memory,
            budget,
        )
    }

    /// Parse files from decompressed block data
    fn parse_files(
        bundle: &mut AssetBundle,
        blocks_data: Vec<u8>,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        let retained_bytes = retained_record_bytes::<BundleFileInfo>(
            bundle.nodes.len(),
            bundle.nodes.iter().map(|node| node.name.len()),
            "bundle file table",
        )?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_bytes(retained_bytes)?;
        let mut files = Vec::new();
        files
            .try_reserve_exact(bundle.nodes.len())
            .map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {} bundle file records: {error}",
                    bundle.nodes.len()
                ))
            })?;
        for node in &bundle.nodes {
            let file_info = BundleFileInfo::new(node.name.clone(), node.offset, node.size);
            files.push(file_info);
        }

        // Publish the prepared data and directory mirror together.
        bundle.set_decompressed_data(blocks_data);
        bundle.files = files;

        Ok(())
    }

    /// Parse directory structure from blocks info data
    fn parse_directory_from_blocks_info(
        bundle: &mut AssetBundle,
        blocks_info_data: &[u8],
        options: &BundleLoadOptions,
        budget: &mut AssetLoadBudget,
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
        let minimum_node_bytes = node_count
            .checked_mul(21)
            .ok_or_else(|| BinaryError::invalid_data("Directory node table size overflow"))?;
        if minimum_node_bytes > reader.remaining() {
            return Err(BinaryError::not_enough_data(
                minimum_node_bytes,
                reader.remaining(),
            ));
        }
        let total_uncompressed = bundle.blocks.iter().try_fold(0_u64, |total, block| {
            total
                .checked_add(u64::from(block.uncompressed_size))
                .ok_or_else(|| BinaryError::invalid_data("Bundle block size total overflow"))
        })?;
        let node_start = usize::try_from(reader.position()).map_err(|_| {
            BinaryError::invalid_data("UnityFS directory start does not fit in usize")
        })?;
        let retained_bytes = preflight_unityfs_directory(
            blocks_info_data,
            node_start,
            node_count,
            total_uncompressed,
        )?;
        consume_directory_budget(node_count, retained_bytes, budget)?;
        let mut nodes = Vec::new();
        nodes.try_reserve_exact(node_count).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {node_count} directory nodes: {error}"
            ))
        })?;

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
            nodes.push(node);
        }
        bundle.nodes = nodes;

        Ok(())
    }

    /// Parse legacy bundle directory
    fn parse_legacy_directory(
        bundle: &mut AssetBundle,
        directory_data: &[u8],
        header_size: usize,
        options: &BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        let _ = header_size; // legacy offsets are relative to the uncompressed blob

        let mut dir_reader = BinaryReader::new(directory_data, ByteOrder::Big);

        // Read file count
        let file_count_i32 = dir_reader.read_i32()?;
        if file_count_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative legacy bundle file count: {}",
                file_count_i32
            )));
        }
        let file_count = usize::try_from(file_count_i32)
            .map_err(|_| BinaryError::invalid_data("Negative legacy bundle file count"))?;
        if file_count > options.max_nodes {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "Legacy bundle file count {} exceeds limit {}",
                file_count, options.max_nodes
            )));
        }
        let minimum_bytes = file_count
            .checked_mul(9)
            .ok_or_else(|| BinaryError::invalid_data("Legacy file table size overflow"))?;
        if minimum_bytes > dir_reader.remaining() {
            return Err(BinaryError::not_enough_data(
                minimum_bytes,
                dir_reader.remaining(),
            ));
        }
        let directory_start = usize::try_from(dir_reader.position()).map_err(|_| {
            BinaryError::invalid_data("Legacy bundle directory start does not fit in usize")
        })?;
        let retained_bytes =
            preflight_legacy_directory(directory_data, directory_start, file_count)?;
        consume_directory_budget(file_count, retained_bytes, budget)?;
        let mut nodes = Vec::new();
        nodes.try_reserve_exact(file_count).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {file_count} legacy directory nodes: {error}"
            ))
        })?;
        let mut files = Vec::new();
        files.try_reserve_exact(file_count).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {file_count} legacy file records: {error}"
            ))
        })?;

        // Read file entries
        for _ in 0..file_count {
            let name = dir_reader.read_cstring()?;
            let offset = dir_reader.read_u32()? as u64;
            let size = dir_reader.read_u32()? as u64;

            let file_info = BundleFileInfo::new(name.clone(), offset, size);
            files.push(file_info);

            // Also create a directory node for consistency
            let node = DirectoryNode::new(name, offset, size, 0x4); // Flag 0x4 = file
            nodes.push(node);
        }
        bundle.nodes = nodes;
        bundle.files = files;

        Ok(())
    }

    /// Load assets from the bundle files
    fn load_assets(
        bundle: &mut AssetBundle,
        options: &BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
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

            let end = node.offset.checked_add(node.size).ok_or_else(|| {
                BinaryError::invalid_data(format!(
                    "Bundle node '{}' offset+size overflow",
                    node.name
                ))
            })?;
            if end > visible_len {
                return Err(BinaryError::invalid_data(format!(
                    "Bundle node '{}' exceeds decompressed data: end {} > {}",
                    node.name, end, visible_len
                )));
            }

            if let Some(max_memory) = options.max_memory
                && node.size > max_memory as u64
            {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Bundle node '{}' size {} exceeds max_memory {}",
                    node.name, node.size, max_memory
                )));
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
            match crate::asset::SerializedFileParser::from_shared_range_with_budget(
                backing.clone(),
                abs_start..abs_end,
                budget,
            ) {
                Ok(serialized_file) => {
                    bundle.assets.try_reserve(1).map_err(|error| {
                        BinaryError::memory_error(format!(
                            "Failed to reserve a bundle asset: {error}"
                        ))
                    })?;
                    bundle.asset_names.try_reserve(1).map_err(|error| {
                        BinaryError::memory_error(format!(
                            "Failed to reserve a bundle asset name: {error}"
                        ))
                    })?;
                    bundle.assets.push(serialized_file);
                    bundle.asset_names.push(node.name.clone());
                }
                Err(error) if error.is_resource_error() => return Err(error),
                Err(_) => {}
            }
        }

        Ok(())
    }

    /// Estimate parsing complexity
    pub fn estimate_complexity(data: &[u8]) -> Result<ParsingComplexity> {
        let mut budget = AssetLoadBudget::default();
        Self::estimate_complexity_with_budget(data, &mut budget)
    }

    pub fn estimate_complexity_with_budget(
        data: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<ParsingComplexity> {
        budget.consume_bytes(u64::try_from(data.len()).map_err(|_| {
            BinaryError::invalid_data("Bundle source length does not fit in u64")
        })?)?;
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

fn consume_directory_budget(
    count: usize,
    retained_bytes: u64,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    let count = u64::try_from(count)
        .map_err(|_| BinaryError::invalid_data("Bundle member count does not fit in u64"))?;
    budget.check_entries(count)?;
    budget.check_members(count)?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(count)?;
    budget.consume_members(count)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(())
}

fn retained_record_bytes<T>(
    count: usize,
    name_lengths: impl IntoIterator<Item = usize>,
    resource: &'static str,
) -> Result<u64> {
    let mut bytes = size_of::<T>()
        .checked_mul(count)
        .ok_or_else(|| BinaryError::invalid_data(format!("{resource} size overflow")))?;
    for name_length in name_lengths {
        bytes = bytes
            .checked_add(name_length)
            .ok_or_else(|| BinaryError::invalid_data(format!("{resource} name size overflow")))?;
    }
    u64::try_from(bytes)
        .map_err(|_| BinaryError::invalid_data(format!("{resource} size does not fit in u64")))
}

fn preflight_unityfs_directory(
    data: &[u8],
    start: usize,
    node_count: usize,
    total_uncompressed: u64,
) -> Result<u64> {
    let mut cursor = start;
    let mut name_bytes = 0_usize;
    for _ in 0..node_count {
        let fixed_end = cursor
            .checked_add(20)
            .ok_or_else(|| BinaryError::invalid_data("Directory node header overflow"))?;
        if fixed_end > data.len() {
            return Err(BinaryError::not_enough_data(fixed_end, data.len()));
        }
        let offset = bundle_i64_at(data, cursor, "directory node offset")?;
        let size = bundle_i64_at(data, cursor + 8, "directory node size")?;
        if offset < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative directory node offset: {offset}"
            )));
        }
        if size < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative directory node size: {size}"
            )));
        }
        let end = u64::try_from(offset)
            .map_err(|_| BinaryError::invalid_data("Negative directory node offset"))?
            .checked_add(
                u64::try_from(size)
                    .map_err(|_| BinaryError::invalid_data("Negative directory node size"))?,
            )
            .ok_or_else(|| BinaryError::invalid_data("Directory node offset+size overflow"))?;
        if end > total_uncompressed {
            return Err(BinaryError::invalid_data(format!(
                "Directory node exceeds decompressed data: end {end} > {total_uncompressed}"
            )));
        }
        let (name_length, next) = preflight_bundle_cstring(data, fixed_end)?;
        name_bytes = name_bytes
            .checked_add(name_length)
            .ok_or_else(|| BinaryError::invalid_data("Directory node name size overflow"))?;
        cursor = next;
    }

    retained_directory_bytes(
        size_of::<DirectoryNode>(),
        node_count,
        name_bytes,
        1,
        "UnityFS directory",
    )
}

fn preflight_legacy_directory(data: &[u8], start: usize, file_count: usize) -> Result<u64> {
    let mut cursor = start;
    let mut name_bytes = 0_usize;
    for _ in 0..file_count {
        let (name_length, after_name) = preflight_bundle_cstring(data, cursor)?;
        let fixed_end = after_name
            .checked_add(8)
            .ok_or_else(|| BinaryError::invalid_data("Legacy file entry range overflow"))?;
        if fixed_end > data.len() {
            return Err(BinaryError::not_enough_data(fixed_end, data.len()));
        }
        name_bytes = name_bytes
            .checked_add(name_length)
            .ok_or_else(|| BinaryError::invalid_data("Legacy file name size overflow"))?;
        cursor = fixed_end;
    }

    let record_bytes = size_of::<DirectoryNode>()
        .checked_add(size_of::<BundleFileInfo>())
        .ok_or_else(|| BinaryError::invalid_data("Legacy directory record size overflow"))?;
    retained_directory_bytes(
        record_bytes,
        file_count,
        name_bytes,
        2,
        "legacy bundle directory",
    )
}

fn retained_directory_bytes(
    record_bytes: usize,
    count: usize,
    name_bytes: usize,
    name_copies: usize,
    resource: &'static str,
) -> Result<u64> {
    let table_bytes = record_bytes
        .checked_mul(count)
        .ok_or_else(|| BinaryError::invalid_data(format!("{resource} table size overflow")))?;
    let names = name_bytes
        .checked_mul(name_copies)
        .ok_or_else(|| BinaryError::invalid_data(format!("{resource} name size overflow")))?;
    let retained = table_bytes
        .checked_add(names)
        .ok_or_else(|| BinaryError::invalid_data(format!("{resource} retained size overflow")))?;
    u64::try_from(retained).map_err(|_| {
        BinaryError::invalid_data(format!("{resource} retained size does not fit in u64"))
    })
}

fn preflight_bundle_cstring(data: &[u8], start: usize) -> Result<(usize, usize)> {
    let remaining = data
        .get(start..)
        .ok_or_else(|| BinaryError::not_enough_data(start, data.len()))?;
    let scan_len = remaining
        .len()
        .min(BinaryReader::DEFAULT_MAX_STRING_LEN.saturating_add(1));
    let Some(length) = remaining[..scan_len].iter().position(|byte| *byte == 0) else {
        return if remaining.len() > BinaryReader::DEFAULT_MAX_STRING_LEN {
            Err(BinaryError::invalid_data(format!(
                "C string exceeds maximum length {}",
                BinaryReader::DEFAULT_MAX_STRING_LEN
            )))
        } else {
            Err(BinaryError::invalid_data(
                "unterminated C string in bounded input",
            ))
        };
    };
    std::str::from_utf8(&remaining[..length])?;
    let next = start
        .checked_add(length)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| BinaryError::invalid_data("C string range overflow"))?;
    Ok((length, next))
}

fn bundle_i64_at(data: &[u8], offset: usize, field: &'static str) -> Result<i64> {
    let end = offset
        .checked_add(size_of::<i64>())
        .ok_or_else(|| BinaryError::invalid_data(format!("{field} range overflow")))?;
    let bytes: [u8; 8] = data
        .get(offset..end)
        .ok_or_else(|| BinaryError::not_enough_data(end, data.len()))?
        .try_into()
        .map_err(|_| BinaryError::invalid_data(format!("Invalid {field} width")))?;
    Ok(i64::from_be_bytes(bytes))
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
    use unity_asset_core::{AssetLoadLimits, BudgetError};

    fn blocks_info_with_nodes(names: &[&str]) -> Vec<u8> {
        let mut bytes = vec![0_u8; 16];
        bytes.extend_from_slice(&0_i32.to_be_bytes());
        bytes.extend_from_slice(
            &i32::try_from(names.len())
                .expect("test node count fits in i32")
                .to_be_bytes(),
        );
        for name in names {
            bytes.extend_from_slice(&0_i64.to_be_bytes());
            bytes.extend_from_slice(&0_i64.to_be_bytes());
            bytes.extend_from_slice(&0x4_u32.to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    fn legacy_directory_with_files(names: &[&str]) -> Vec<u8> {
        let mut bytes = i32::try_from(names.len())
            .expect("test file count fits in i32")
            .to_be_bytes()
            .to_vec();
        for name in names {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&0_u32.to_be_bytes());
            bytes.extend_from_slice(&0_u32.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn test_parser_creation() {
        // Basic test to ensure parser can be created
        // In practice, you'd need actual bundle data to test parsing
        let _dummy = 1 + 1;
        assert_eq!(_dummy, 2);
    }

    #[test]
    fn load_assets_rejects_out_of_bounds_node() {
        let header = BundleHeader {
            signature: "UnityFS".to_string(),
            ..Default::default()
        };
        let mut bundle = AssetBundle::new(header, vec![0u8; 8]);
        bundle
            .nodes
            .push(DirectoryNode::new("a.assets".to_string(), 1024, 4, 0x4));

        let mut budget = AssetLoadBudget::default();
        let err =
            BundleParser::load_assets(&mut bundle, &BundleLoadOptions::default(), &mut budget)
                .unwrap_err();
        assert!(matches!(err, BinaryError::InvalidData(_)));
    }

    #[test]
    fn bundle_source_is_charged_before_header_parsing() {
        let bytes = b"UnityFS\0".to_vec();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(bytes.len() - 1).unwrap(),
            ..Default::default()
        })
        .unwrap();

        let error = BundleParser::from_bytes_with_budget(bytes, &mut budget).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn unityfs_member_limit_precedes_directory_allocations() {
        let data = blocks_info_with_nodes(&["left", "right"]);
        let mut bundle = AssetBundle::new_empty(BundleHeader::default());
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = BundleParser::parse_directory_from_blocks_info(
            &mut bundle,
            &data,
            &BundleLoadOptions::default(),
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "members",
                limit: 1,
                requested: 2,
            })
        ));
        assert!(bundle.nodes.is_empty());
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn unityfs_retained_node_bytes_are_preflighted_before_allocation() {
        let names = ["left", "right"];
        let data = blocks_info_with_nodes(&names);
        let retained = u64::try_from(
            names.len() * std::mem::size_of::<DirectoryNode>()
                + names.iter().map(|name| name.len()).sum::<usize>(),
        )
        .unwrap();
        let mut bundle = AssetBundle::new_empty(BundleHeader::default());
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: retained - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = BundleParser::parse_directory_from_blocks_info(
            &mut bundle,
            &data,
            &BundleLoadOptions::default(),
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == retained - 1 && requested == retained
        ));
        assert!(bundle.nodes.is_empty());
        assert_eq!(budget.usage().bytes, 0);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn legacy_member_limit_precedes_both_directory_tables() {
        let data = legacy_directory_with_files(&["left", "right"]);
        let mut bundle = AssetBundle::new_empty(BundleHeader::default());
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = BundleParser::parse_legacy_directory(
            &mut bundle,
            &data,
            0,
            &BundleLoadOptions::default(),
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "members",
                limit: 1,
                requested: 2,
            })
        ));
        assert!(bundle.nodes.is_empty());
        assert!(bundle.files.is_empty());
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().entries, 0);
    }
}
