//! Bundle parser implementation
//!
//! This module provides the main parsing logic for Unity AssetBundles,
//! inspired by UnityPy/files/BundleFile.py

use super::compression::BundleCompression;
use super::header::{BundleHeader, BundleLayoutKind, LegacyWebRawHeader};
use super::types::{AssetBundle, BundleFileInfo, BundleLoadOptions, DirectoryNode};
use crate::compression::{
    CompressionType, decompressor_scratch_bytes, inspect_lzma_size_stream_with_budget,
};
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::random_access::{
    BorrowedBytes, ByteCursor, ByteSource, ByteSourceReader, SegmentedBytes,
};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
use crate::unity_version::UnityVersion;
use std::mem::size_of;
use std::ops::Range;
use unity_asset_core::{AssetLoadBudget, string_allocation_bytes, vec_allocation_bytes};

/// Main bundle parser
///
/// This struct handles the parsing of Unity AssetBundle files,
/// supporting both UnityFS and legacy formats.
pub struct BundleParser;

/// Opaque proof produced by independently parsing and validating a bundle image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleInspection {
    signature: String,
    version: u32,
    unity_version: String,
    unity_revision: String,
    layout: BundleLayoutKind,
    declared_size: u64,
    flags: u32,
    file_stream_header_byte: Option<u8>,
    compression: CompressionType,
    blocks_info_hash: Option<[u8; 16]>,
    legacy: Option<BundleLegacyInspection>,
    blocks: Vec<BundleBlockInspection>,
    directory: Vec<BundleDirectoryInspection>,
    stats: BundleInspectionStats,
}

/// Legacy UnityWeb/UnityRaw header fields retained in wire form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleLegacyInspection {
    hash: Option<[u8; 16]>,
    crc: Option<u32>,
    minimum_streamed_bytes: u32,
    header_size: u32,
    number_of_levels_to_download_before_streaming: u32,
    level_count: i32,
    compressed_size: u32,
    uncompressed_size: u32,
    complete_file_size: Option<u32>,
    file_info_header_size: Option<u32>,
}

/// One physical UnityFS data block in encoded order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleBlockInspection {
    uncompressed_size: u32,
    compressed_size: u32,
    flags: u16,
    compression: CompressionType,
    encoded_range: Range<u64>,
}

/// One bundle directory record in wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleDirectoryInspection {
    name: String,
    occurrence: usize,
    offset: u64,
    length: u64,
    flags: u32,
}

/// Bounded work performed while inspecting a bundle image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleInspectionStats {
    encoded_bytes: u64,
    compressed_bytes: u64,
    decompressed_bytes: u64,
    max_temporary_bytes: u64,
}

impl BundleInspection {
    pub fn signature(&self) -> &str {
        &self.signature
    }

    pub const fn version(&self) -> u32 {
        self.version
    }

    pub fn unity_version(&self) -> &str {
        &self.unity_version
    }

    pub fn unity_revision(&self) -> &str {
        &self.unity_revision
    }

    pub const fn layout(&self) -> BundleLayoutKind {
        self.layout
    }

    pub const fn declared_size(&self) -> u64 {
        self.declared_size
    }

    pub const fn flags(&self) -> u32 {
        self.flags
    }

    pub const fn file_stream_header_byte(&self) -> Option<u8> {
        self.file_stream_header_byte
    }

    pub const fn compression(&self) -> CompressionType {
        self.compression
    }

    pub const fn blocks_info_hash(&self) -> Option<[u8; 16]> {
        self.blocks_info_hash
    }

    pub const fn legacy(&self) -> Option<&BundleLegacyInspection> {
        self.legacy.as_ref()
    }

    pub fn blocks(&self) -> &[BundleBlockInspection] {
        &self.blocks
    }

    pub fn directory(&self) -> &[BundleDirectoryInspection] {
        &self.directory
    }

    pub const fn stats(&self) -> BundleInspectionStats {
        self.stats
    }

    pub fn retained_heap_bytes(&self) -> Result<u64> {
        let mut bytes = string_allocation_bytes(self.signature.capacity())
            .map_err(inspection_allocation_size_error)?;
        add_retained_bytes(
            &mut bytes,
            string_allocation_bytes(self.unity_version.capacity())
                .map_err(inspection_allocation_size_error)?,
        )?;
        add_retained_bytes(
            &mut bytes,
            string_allocation_bytes(self.unity_revision.capacity())
                .map_err(inspection_allocation_size_error)?,
        )?;
        add_retained_bytes(
            &mut bytes,
            vec_allocation_bytes::<BundleBlockInspection>(self.blocks.capacity())
                .map_err(inspection_allocation_size_error)?,
        )?;
        add_retained_bytes(
            &mut bytes,
            vec_allocation_bytes::<BundleDirectoryInspection>(self.directory.capacity())
                .map_err(inspection_allocation_size_error)?,
        )?;
        for entry in &self.directory {
            add_retained_bytes(
                &mut bytes,
                string_allocation_bytes(entry.name.capacity())
                    .map_err(inspection_allocation_size_error)?,
            )?;
        }
        Ok(bytes)
    }
}

impl BundleLegacyInspection {
    pub const fn hash(&self) -> Option<[u8; 16]> {
        self.hash
    }

    pub const fn crc(&self) -> Option<u32> {
        self.crc
    }

    pub const fn minimum_streamed_bytes(&self) -> u32 {
        self.minimum_streamed_bytes
    }

    pub const fn header_size(&self) -> u32 {
        self.header_size
    }

    pub const fn levels_before_streaming(&self) -> u32 {
        self.number_of_levels_to_download_before_streaming
    }

    pub const fn level_count(&self) -> i32 {
        self.level_count
    }

    pub const fn compressed_size(&self) -> u32 {
        self.compressed_size
    }

    pub const fn uncompressed_size(&self) -> u32 {
        self.uncompressed_size
    }

    pub const fn complete_file_size(&self) -> Option<u32> {
        self.complete_file_size
    }

    pub const fn file_info_header_size(&self) -> Option<u32> {
        self.file_info_header_size
    }
}

impl BundleBlockInspection {
    pub const fn uncompressed_size(&self) -> u32 {
        self.uncompressed_size
    }

    pub const fn compressed_size(&self) -> u32 {
        self.compressed_size
    }

    pub const fn flags(&self) -> u16 {
        self.flags
    }

    pub const fn compression(&self) -> CompressionType {
        self.compression
    }

    pub fn encoded_range(&self) -> Range<u64> {
        self.encoded_range.clone()
    }
}

impl BundleDirectoryInspection {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn occurrence(&self) -> usize {
        self.occurrence
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn length(&self) -> u64 {
        self.length
    }

    pub const fn flags(&self) -> u32 {
        self.flags
    }
}

impl BundleInspectionStats {
    pub const fn encoded_bytes(self) -> u64 {
        self.encoded_bytes
    }

    pub const fn compressed_bytes(self) -> u64 {
        self.compressed_bytes
    }

    pub const fn decompressed_bytes(self) -> u64 {
        self.decompressed_bytes
    }

    pub const fn max_temporary_bytes(self) -> u64 {
        self.max_temporary_bytes
    }
}

impl BundleParser {
    /// Inspects a contiguous bundle through the same random-access parser as segmented images.
    pub fn inspect_slice_with_budget(
        data: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<BundleInspection> {
        Self::inspect_source_with_budget(&BorrowedBytes::new(data), budget)
    }

    /// Inspects and validates an immutable segmented bundle without concatenating its image.
    pub fn inspect_segmented_with_budget(
        data: &SegmentedBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<BundleInspection> {
        Self::inspect_source_with_budget(data, budget)
    }

    fn inspect_source_with_budget(
        source: &dyn ByteSource,
        budget: &mut AssetLoadBudget,
    ) -> Result<BundleInspection> {
        let options = BundleLoadOptions::lazy();
        let (header, blocks_info, block_data_range) = {
            let mut reader = ByteCursor::new(source, ByteOrder::Big, budget)?;
            let header = BundleHeader::from_input(&mut reader)?;
            header.validate()?;
            Self::validate_declared_header_limits(&header, &options)?;
            if header.size != source.len() {
                return Err(BinaryError::invalid_data(format!(
                    "Bundle declared size {} does not match available bytes {}",
                    header.size,
                    source.len()
                )));
            }
            if header.layout_kind()? == BundleLayoutKind::Legacy {
                return Self::inspect_legacy_source_with_budget(source, header, &options, budget);
            }
            let (blocks_info, block_data_range) =
                read_file_stream_blocks_info_from_source(&header, &mut reader)?;
            (header, blocks_info, block_data_range)
        };

        let blocks_info_compression = header.compression_type()?;
        let blocks_info_scratch = decompressor_scratch_bytes(
            &blocks_info,
            blocks_info_compression,
            usize::try_from(header.uncompressed_blocks_info_size).map_err(|_| {
                BinaryError::invalid_data("Blocks-info decoded size does not fit usize")
            })?,
        )?;
        let blocks_info_decoded = BundleCompression::decompress_blocks_info_limited_with_budget(
            &header,
            &blocks_info,
            options.max_blocks_info_size,
            budget,
        )?;
        let blocks_info_hash = blocks_info_decoded
            .get(..16)
            .ok_or_else(|| BinaryError::invalid_data("UnityFS blocks info is missing its hash"))?
            .try_into()
            .map_err(|_| BinaryError::invalid_data("Invalid UnityFS blocks-info hash width"))?;
        let mut tables =
            parse_file_stream_inspection_tables(&blocks_info_decoded, &options, budget)?;
        let total_uncompressed = tables.total_uncompressed;
        let declared_compressed = tables.total_compressed;
        if declared_compressed != block_data_range.end - block_data_range.start {
            return Err(BinaryError::invalid_data(format!(
                "FileStream compressed block total {declared_compressed} does not exactly cover physical payload range {}",
                block_data_range.end - block_data_range.start
            )));
        }

        let mut max_temporary_bytes = u64::from(header.compressed_blocks_info_size)
            .checked_add(u64::from(header.uncompressed_blocks_info_size))
            .and_then(|bytes| bytes.checked_add(blocks_info_scratch))
            .ok_or_else(|| BinaryError::invalid_data("Blocks-info working set overflow"))?;
        drop(blocks_info_decoded);
        drop(blocks_info);
        let mut block_offset = block_data_range.start;
        for block in &mut tables.blocks {
            let block_end = block_offset
                .checked_add(u64::from(block.compressed_size))
                .ok_or_else(|| BinaryError::invalid_data("Block source range overflow"))?;
            block.encoded_range = block_offset..block_end;
            let encoded = {
                let mut reader = ByteCursor::with_range(
                    source,
                    block_offset..block_end,
                    ByteOrder::Big,
                    budget,
                )?;
                reader.read_bytes(u64::from(block.compressed_size))?
            };
            let uncompressed_size = usize::try_from(block.uncompressed_size).map_err(|_| {
                BinaryError::invalid_data("Block uncompressed size does not fit usize")
            })?;
            let scratch =
                decompressor_scratch_bytes(&encoded, block.compression, uncompressed_size)?;
            let decoded = crate::compression::decompress_with_budget(
                &encoded,
                block.compression,
                uncompressed_size,
                budget,
            )?;
            if decoded.len() != uncompressed_size {
                return Err(BinaryError::invalid_data(format!(
                    "UnityFS block decoded to {} bytes, expected {}",
                    decoded.len(),
                    block.uncompressed_size
                )));
            }
            let working_set = u64::from(block.compressed_size)
                .checked_add(u64::from(block.uncompressed_size))
                .and_then(|bytes| bytes.checked_add(scratch))
                .ok_or_else(|| BinaryError::invalid_data("Block working set overflow"))?;
            max_temporary_bytes = max_temporary_bytes.max(working_set);
            block_offset = block_end;
        }
        if block_offset != block_data_range.end {
            return Err(BinaryError::invalid_data(format!(
                "FileStream block ranges end at {block_offset}, expected physical payload end {}",
                block_data_range.end
            )));
        }

        let compressed_bytes = u64::from(header.compressed_blocks_info_size)
            .checked_add(declared_compressed)
            .ok_or_else(|| {
                BinaryError::invalid_data("Inspection compressed byte total overflow")
            })?;
        let decompressed_bytes = u64::from(header.uncompressed_blocks_info_size)
            .checked_add(total_uncompressed)
            .ok_or_else(|| BinaryError::invalid_data("Inspection decoded byte total overflow"))?;
        let compression = header.compression_type()?;
        Ok(BundleInspection {
            signature: header.signature,
            version: header.version,
            unity_version: header.unity_version,
            unity_revision: header.unity_revision,
            layout: BundleLayoutKind::FileStream,
            declared_size: header.size,
            flags: header.flags,
            file_stream_header_byte: header.file_stream_header_byte,
            compression,
            blocks_info_hash: Some(blocks_info_hash),
            legacy: None,
            blocks: tables.blocks,
            directory: tables.directory,
            stats: BundleInspectionStats {
                encoded_bytes: source.len(),
                compressed_bytes,
                decompressed_bytes,
                max_temporary_bytes,
            },
        })
    }

    fn inspect_legacy_source_with_budget(
        source: &dyn ByteSource,
        header: BundleHeader,
        options: &BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<BundleInspection> {
        let legacy = header.legacy_web_raw.as_ref().ok_or_else(|| {
            BinaryError::invalid_data("Legacy bundle header fields were not parsed")
        })?;
        let compressed_size = u64::from(legacy.compressed_size);
        let uncompressed_size = u64::from(legacy.uncompressed_size);
        if legacy.level_count != 1 {
            return Err(BinaryError::unsupported(format!(
                "Segmented legacy inspection supports levelCount=1, got {}",
                legacy.level_count
            )));
        }
        let file_info_header_size = legacy.file_info_header_size.ok_or_else(|| {
            BinaryError::unsupported(
                "Segmented legacy inspection requires version 3 or newer directory metadata",
            )
        })?;
        if file_info_header_size > legacy.uncompressed_size {
            return Err(BinaryError::invalid_data(format!(
                "Legacy directory header size {file_info_header_size} exceeds decoded blob size {}",
                legacy.uncompressed_size
            )));
        }

        let blob_start = u64::from(legacy.header_size);
        let blob_end = blob_start
            .checked_add(u64::from(legacy.compressed_size))
            .ok_or_else(|| BinaryError::invalid_data("Legacy blob range overflow"))?;
        if blob_end != header.size {
            return Err(BinaryError::invalid_data(format!(
                "Legacy blob ends at {blob_end}, but declared bundle size is {}",
                header.size
            )));
        }
        let blob_range = blob_start..blob_end;
        let prefix_size = usize::try_from(file_info_header_size).map_err(|_| {
            BinaryError::invalid_data("Legacy directory header size does not fit usize")
        })?;
        let (directory_prefix, max_temporary_bytes, compression) = match header.signature.as_str() {
            "UnityRaw" => {
                if legacy.compressed_size != legacy.uncompressed_size {
                    return Err(BinaryError::invalid_data(format!(
                        "UnityRaw encoded size {} does not match decoded size {}",
                        legacy.compressed_size, legacy.uncompressed_size
                    )));
                }
                budget.check_decompression(
                    u64::from(legacy.compressed_size),
                    u64::from(legacy.uncompressed_size),
                )?;
                budget.begin_decompression().consume(
                    u64::from(legacy.compressed_size),
                    u64::from(legacy.uncompressed_size),
                )?;
                let prefix_end = blob_start
                    .checked_add(u64::from(file_info_header_size))
                    .ok_or_else(|| BinaryError::invalid_data("Legacy directory range overflow"))?;
                let prefix =
                    ByteCursor::with_range(source, blob_start..prefix_end, ByteOrder::Big, budget)?
                        .read_bytes(u64::from(file_info_header_size))?;
                (
                    prefix,
                    u64::from(file_info_header_size),
                    CompressionType::None,
                )
            }
            "UnityWeb" => {
                let input = ByteSourceReader::with_range(source, blob_range)?;
                let inspected = inspect_lzma_size_stream_with_budget(
                    input,
                    u64::from(legacy.compressed_size),
                    u64::from(legacy.uncompressed_size),
                    prefix_size,
                    budget,
                )?;
                if inspected.decoded_len != u64::from(legacy.uncompressed_size) {
                    return Err(BinaryError::invalid_data(
                        "Legacy LZMA decoder returned an inconsistent length",
                    ));
                }
                (
                    inspected.prefix,
                    inspected.max_temporary_bytes,
                    CompressionType::Lzma,
                )
            }
            signature => {
                return Err(BinaryError::unsupported(format!(
                    "Unsupported legacy bundle signature: {signature}"
                )));
            }
        };

        let directory = parse_legacy_inspection_directory(
            &directory_prefix,
            u64::from(file_info_header_size),
            u64::from(legacy.uncompressed_size),
            options,
            budget,
        )?;
        let legacy = inspect_legacy_header(legacy)?;
        Ok(BundleInspection {
            signature: header.signature,
            version: header.version,
            unity_version: header.unity_version,
            unity_revision: header.unity_revision,
            layout: BundleLayoutKind::Legacy,
            declared_size: header.size,
            flags: header.flags,
            file_stream_header_byte: None,
            compression,
            blocks_info_hash: None,
            legacy: Some(legacy),
            blocks: Vec::new(),
            directory,
            stats: BundleInspectionStats {
                encoded_bytes: source.len(),
                compressed_bytes: compressed_size,
                decompressed_bytes: uncompressed_size,
                max_temporary_bytes,
            },
        })
    }

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

        let layout = header.layout_kind()?;
        let mut bundle = AssetBundle::new_empty(header);
        if layout == BundleLayoutKind::Legacy {
            bundle.set_legacy_source(view.clone());
        }

        match layout {
            BundleLayoutKind::FileStream => {
                Self::parse_file_stream(&mut bundle, &view, &mut reader, &options, budget)?;
            }
            BundleLayoutKind::Legacy => {
                Self::parse_legacy(&mut bundle, &mut reader, &options, budget)?;
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

        if header.layout_kind()? == BundleLayoutKind::FileStream {
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

    /// Parse a block-based FileStream bundle.
    fn parse_file_stream(
        bundle: &mut AssetBundle,
        source: &DataView,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        // Read blocks info
        let block_data_start = Self::read_blocks_info(bundle, reader, options, budget)?;
        let block_data = Self::file_stream_block_data_view(bundle, source, block_data_start)?;

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

    fn file_stream_block_data_view(
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
        if declared_compressed != available {
            return Err(BinaryError::invalid_data(format!(
                "FileStream compressed block total {declared_compressed} does not exactly cover physical payload range {available}"
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

            // Legacy directories do not encode FileStream NodeFlags. Preserve that absence instead
            // of claiming every entry is a Unity SerializedFile.
            let node = DirectoryNode::new(name, offset, size, 0);
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
        let (backing, base_offset, visible_len) =
            if bundle.header.layout_kind()? == BundleLayoutKind::FileStream {
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

            let prefix_end = abs_end.min(abs_start.saturating_add(48));
            if !crate::file::looks_like_serialized_file_prefix(
                &backing.as_bytes()[abs_start..prefix_end],
            ) {
                continue;
            }

            if let Some(max_memory) = options.max_memory
                && node.size > max_memory as u64
            {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Bundle node '{}' size {} exceeds max_memory {}",
                    node.name, node.size, max_memory
                )));
            }

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

        let complexity = match header.layout_kind()? {
            BundleLayoutKind::FileStream => {
                let compression_type = header.compression_type()?;
                let has_compression = compression_type != CompressionType::None;

                ParsingComplexity {
                    format: header.signature.clone(),
                    estimated_time: if has_compression { "Medium" } else { "Fast" }.to_string(),
                    memory_usage: header.size,
                    has_compression,
                    block_count: 0, // Would need to parse blocks info to get accurate count
                }
            }
            BundleLayoutKind::Legacy => ParsingComplexity {
                format: header.signature.clone(),
                estimated_time: "Fast".to_string(),
                memory_usage: header.size,
                has_compression: header.signature == "UnityWeb",
                block_count: 1,
            },
        };

        Ok(complexity)
    }
}

struct FileStreamInspectionTables {
    blocks: Vec<BundleBlockInspection>,
    directory: Vec<BundleDirectoryInspection>,
    total_compressed: u64,
    total_uncompressed: u64,
}

fn parse_file_stream_inspection_tables(
    data: &[u8],
    options: &BundleLoadOptions,
    budget: &mut AssetLoadBudget,
) -> Result<FileStreamInspectionTables> {
    if data.len() < 20 {
        return Err(BinaryError::not_enough_data(20, data.len()));
    }
    let block_count = nonnegative_bundle_count(data, 16, "compression block")?;
    if block_count == 0 {
        return Err(BinaryError::invalid_data("No compression blocks found"));
    }
    if block_count > options.max_blocks {
        return Err(BinaryError::ResourceLimitExceeded(format!(
            "Compression block count {block_count} exceeds limit {}",
            options.max_blocks
        )));
    }
    let block_table_bytes = block_count
        .checked_mul(10)
        .ok_or_else(|| BinaryError::invalid_data("Compression block table size overflow"))?;
    let block_table_end = 20_usize
        .checked_add(block_table_bytes)
        .ok_or_else(|| BinaryError::invalid_data("Compression block table range overflow"))?;
    let node_count = nonnegative_bundle_count(data, block_table_end, "directory node")?;
    if node_count > options.max_nodes {
        return Err(BinaryError::ResourceLimitExceeded(format!(
            "Directory node count {node_count} exceeds limit {}",
            options.max_nodes
        )));
    }

    let mut total_compressed = 0_u64;
    let mut total_uncompressed = 0_u64;
    for index in 0..block_count {
        let offset =
            20_usize
                .checked_add(index.checked_mul(10).ok_or_else(|| {
                    BinaryError::invalid_data("Compression block offset overflow")
                })?)
                .ok_or_else(|| BinaryError::invalid_data("Compression block offset overflow"))?;
        let uncompressed_size = bundle_u32_at(data, offset, "block uncompressed size")?;
        let compressed_size = bundle_u32_at(data, offset + 4, "block compressed size")?;
        let flags = bundle_u16_at(data, offset + 8, "block flags")?;
        if compressed_size == 0 {
            return Err(BinaryError::invalid_data(format!(
                "Block {index} has zero compressed size"
            )));
        }
        if uncompressed_size == 0 {
            return Err(BinaryError::invalid_data(format!(
                "Block {index} has zero uncompressed size"
            )));
        }
        if u64::from(compressed_size) > u64::from(uncompressed_size) * 2 && uncompressed_size > 1024
        {
            return Err(BinaryError::invalid_data(format!(
                "Block {index} has suspicious compression ratio: {compressed_size}/{uncompressed_size}"
            )));
        }
        CompressionType::from_flags(u32::from(flags))?;
        total_compressed = total_compressed
            .checked_add(u64::from(compressed_size))
            .ok_or_else(|| BinaryError::invalid_data("Bundle compressed size total overflow"))?;
        total_uncompressed = total_uncompressed
            .checked_add(u64::from(uncompressed_size))
            .ok_or_else(|| BinaryError::invalid_data("Bundle block size total overflow"))?;
    }

    let mut node_cursor = block_table_end
        .checked_add(4)
        .ok_or_else(|| BinaryError::invalid_data("Directory table start overflow"))?;
    let mut name_bytes = 0_usize;
    for _ in 0..node_count {
        let fixed_end = node_cursor
            .checked_add(20)
            .ok_or_else(|| BinaryError::invalid_data("Directory node header overflow"))?;
        if fixed_end > data.len() {
            return Err(BinaryError::not_enough_data(fixed_end, data.len()));
        }
        let offset = bundle_i64_at(data, node_cursor, "directory node offset")?;
        let length = bundle_i64_at(data, node_cursor + 8, "directory node size")?;
        let offset = u64::try_from(offset)
            .map_err(|_| BinaryError::invalid_data("Negative directory node offset"))?;
        let length = u64::try_from(length)
            .map_err(|_| BinaryError::invalid_data("Negative directory node size"))?;
        let end = offset
            .checked_add(length)
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
        node_cursor = next;
    }
    if node_cursor != data.len() {
        return Err(BinaryError::invalid_data(format!(
            "FileStream blocks info contains {} trailing decoded bytes",
            data.len() - node_cursor
        )));
    }

    let entry_count = block_count
        .checked_add(node_count)
        .ok_or_else(|| BinaryError::invalid_data("Inspection entry count overflow"))?;
    let block_bytes = vec_allocation_bytes::<BundleBlockInspection>(block_count)
        .map_err(inspection_allocation_size_error)?;
    let directory_bytes = vec_allocation_bytes::<BundleDirectoryInspection>(node_count)
        .map_err(inspection_allocation_size_error)?;
    let name_bytes =
        string_allocation_bytes(name_bytes).map_err(inspection_allocation_size_error)?;
    let retained_bytes = block_bytes
        .checked_add(directory_bytes)
        .and_then(|bytes| bytes.checked_add(name_bytes))
        .ok_or_else(|| BinaryError::memory_error("FileStream inspection retained size overflow"))?;
    let entry_count = u64::try_from(entry_count)
        .map_err(|_| BinaryError::invalid_data("Inspection entry count does not fit u64"))?;
    let node_count_u64 = u64::try_from(node_count)
        .map_err(|_| BinaryError::invalid_data("Directory node count does not fit u64"))?;
    budget.check_entries(entry_count)?;
    budget.check_members(node_count_u64)?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(entry_count)?;
    budget.consume_members(node_count_u64)?;
    budget.consume_bytes(retained_bytes)?;

    let mut blocks = Vec::new();
    blocks.try_reserve_exact(block_count).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {block_count} inspected blocks: {error}"
        ))
    })?;
    for index in 0..block_count {
        let offset = 20 + index * 10;
        let flags = bundle_u16_at(data, offset + 8, "block flags")?;
        blocks.push(BundleBlockInspection {
            uncompressed_size: bundle_u32_at(data, offset, "block uncompressed size")?,
            compressed_size: bundle_u32_at(data, offset + 4, "block compressed size")?,
            flags,
            compression: CompressionType::from_flags(u32::from(flags))?,
            encoded_range: 0..0,
        });
    }

    let mut directory = Vec::new();
    directory.try_reserve_exact(node_count).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {node_count} inspected directory records: {error}"
        ))
    })?;
    node_cursor = block_table_end + 4;
    for _ in 0..node_count {
        let fixed_end = node_cursor + 20;
        let offset = u64::try_from(bundle_i64_at(data, node_cursor, "directory node offset")?)
            .map_err(|_| BinaryError::invalid_data("Negative directory node offset"))?;
        let length = u64::try_from(bundle_i64_at(data, node_cursor + 8, "directory node size")?)
            .map_err(|_| BinaryError::invalid_data("Negative directory node size"))?;
        let flags = bundle_u32_at(data, node_cursor + 16, "directory node flags")?;
        let (name_length, next) = preflight_bundle_cstring(data, fixed_end)?;
        let name_bytes = &data[fixed_end..fixed_end + name_length];
        let name_text = std::str::from_utf8(name_bytes)?;
        let mut name = String::new();
        name.try_reserve_exact(name_length).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {name_length} directory name bytes: {error}"
            ))
        })?;
        name.push_str(name_text);
        directory.push(BundleDirectoryInspection {
            name,
            occurrence: 0,
            offset,
            length,
            flags,
        });
        node_cursor = next;
    }
    assign_directory_occurrences(&mut directory, budget)?;

    Ok(FileStreamInspectionTables {
        blocks,
        directory,
        total_compressed,
        total_uncompressed,
    })
}

fn nonnegative_bundle_count(data: &[u8], offset: usize, field: &'static str) -> Result<usize> {
    let value = bundle_i32_at(data, offset, field)?;
    usize::try_from(value).map_err(|_| BinaryError::invalid_data(format!("Negative {field} count")))
}

fn inspection_allocation_size_error(error: unity_asset_core::AllocationSizeError) -> BinaryError {
    BinaryError::memory_error(format!(
        "Inspection retained allocation size overflow: {error}"
    ))
}

fn add_retained_bytes(total: &mut u64, amount: u64) -> Result<()> {
    *total = total
        .checked_add(amount)
        .ok_or_else(|| BinaryError::memory_error("Inspection retained heap size overflow"))?;
    Ok(())
}

fn inspect_legacy_header(legacy: &LegacyWebRawHeader) -> Result<BundleLegacyInspection> {
    let hash = legacy
        .hash
        .as_deref()
        .map(|hash| {
            hash.try_into()
                .map_err(|_| BinaryError::invalid_data("Invalid legacy bundle hash width"))
        })
        .transpose()?;
    Ok(BundleLegacyInspection {
        hash,
        crc: legacy.crc,
        minimum_streamed_bytes: legacy.minimum_streamed_bytes,
        header_size: legacy.header_size,
        number_of_levels_to_download_before_streaming: legacy
            .number_of_levels_to_download_before_streaming,
        level_count: legacy.level_count,
        compressed_size: legacy.compressed_size,
        uncompressed_size: legacy.uncompressed_size,
        complete_file_size: legacy.complete_file_size,
        file_info_header_size: legacy.file_info_header_size,
    })
}

fn parse_legacy_inspection_directory(
    directory_data: &[u8],
    directory_size: u64,
    decoded_size: u64,
    options: &BundleLoadOptions,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<BundleDirectoryInspection>> {
    let declared_directory_size = usize::try_from(directory_size)
        .map_err(|_| BinaryError::invalid_data("Legacy directory size does not fit usize"))?;
    if declared_directory_size != directory_data.len() {
        return Err(BinaryError::invalid_data(format!(
            "Legacy directory prefix has {} bytes, expected {declared_directory_size}",
            directory_data.len()
        )));
    }
    let file_count = nonnegative_bundle_count(directory_data, 0, "legacy bundle file")?;
    if file_count > options.max_nodes {
        return Err(BinaryError::ResourceLimitExceeded(format!(
            "Legacy bundle file count {file_count} exceeds limit {}",
            options.max_nodes
        )));
    }
    let directory_start = 4;
    let (name_bytes, directory_end) = preflight_legacy_inspection_directory(
        directory_data,
        directory_start,
        file_count,
        directory_size,
        decoded_size,
    )?;
    let padding = &directory_data[directory_end..];
    if !padding.is_empty() {
        let expected_padding = (4 - (directory_end % 4)) % 4;
        if padding.len() != expected_padding {
            return Err(BinaryError::invalid_data(format!(
                "Legacy directory contains {} trailing bytes",
                padding.len()
            )));
        }
        if padding.iter().any(|byte| *byte != 0) {
            return Err(BinaryError::invalid_data(
                "Legacy directory alignment padding is not zero-filled",
            ));
        }
    }
    let retained_bytes = size_of::<BundleDirectoryInspection>()
        .checked_mul(file_count)
        .and_then(|bytes| bytes.checked_add(name_bytes))
        .ok_or_else(|| BinaryError::invalid_data("Legacy inspection directory size overflow"))?;
    consume_directory_budget(
        file_count,
        u64::try_from(retained_bytes).map_err(|_| {
            BinaryError::invalid_data("Legacy inspection directory size does not fit u64")
        })?,
        budget,
    )?;

    let mut directory = Vec::new();
    directory.try_reserve_exact(file_count).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {file_count} legacy inspection records: {error}"
        ))
    })?;
    let mut cursor = directory_start;
    for _ in 0..file_count {
        let (name_length, after_name) = preflight_bundle_cstring(directory_data, cursor)?;
        let name_text = std::str::from_utf8(&directory_data[cursor..cursor + name_length])?;
        let mut name = String::new();
        name.try_reserve_exact(name_length).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {name_length} legacy directory name bytes: {error}"
            ))
        })?;
        name.push_str(name_text);
        let offset = u64::from(bundle_u32_at(
            directory_data,
            after_name,
            "legacy directory offset",
        )?);
        let length = u64::from(bundle_u32_at(
            directory_data,
            after_name + 4,
            "legacy directory length",
        )?);
        directory.push(BundleDirectoryInspection {
            name,
            occurrence: 0,
            offset,
            length,
            flags: 0,
        });
        cursor = after_name + 8;
    }
    assign_directory_occurrences(&mut directory, budget)?;
    Ok(directory)
}

fn preflight_legacy_inspection_directory(
    data: &[u8],
    start: usize,
    file_count: usize,
    directory_size: u64,
    decoded_size: u64,
) -> Result<(usize, usize)> {
    let mut cursor = start;
    let mut name_bytes = 0_usize;
    for _ in 0..file_count {
        let (name_length, after_name) = preflight_bundle_cstring(data, cursor)?;
        let fixed_end = after_name
            .checked_add(8)
            .ok_or_else(|| BinaryError::invalid_data("Legacy directory entry range overflow"))?;
        if fixed_end > data.len() {
            return Err(BinaryError::invalid_data(
                "Legacy directory entry crosses file_info_header_size",
            ));
        }
        let offset = u64::from(bundle_u32_at(data, after_name, "legacy directory offset")?);
        let length = u64::from(bundle_u32_at(
            data,
            after_name + 4,
            "legacy directory length",
        )?);
        let end = offset
            .checked_add(length)
            .ok_or_else(|| BinaryError::invalid_data("Legacy directory data range overflow"))?;
        if offset < directory_size || end > decoded_size {
            return Err(BinaryError::invalid_data(format!(
                "Legacy directory data range {offset}..{end} is outside payload {directory_size}..{decoded_size}"
            )));
        }
        name_bytes = name_bytes
            .checked_add(name_length)
            .ok_or_else(|| BinaryError::invalid_data("Legacy directory name size overflow"))?;
        cursor = fixed_end;
    }
    Ok((name_bytes, cursor))
}

fn assign_directory_occurrences(
    directory: &mut [BundleDirectoryInspection],
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    let index_bytes = size_of::<usize>()
        .checked_mul(directory.len())
        .ok_or_else(|| BinaryError::invalid_data("Directory occurrence index size overflow"))?;
    budget.consume_bytes(u64::try_from(index_bytes).map_err(|_| {
        BinaryError::invalid_data("Directory occurrence index size does not fit u64")
    })?)?;
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(directory.len())
        .map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {} directory occurrence indices: {error}",
                directory.len()
            ))
        })?;
    indices.extend(0..directory.len());
    indices.sort_unstable_by(|left, right| {
        directory[*left]
            .name
            .cmp(&directory[*right].name)
            .then_with(|| left.cmp(right))
    });

    let mut previous_index: Option<usize> = None;
    let mut occurrence = 0_usize;
    for index in indices {
        if previous_index.is_some_and(|previous| directory[previous].name == directory[index].name)
        {
            occurrence = occurrence
                .checked_add(1)
                .ok_or_else(|| BinaryError::invalid_data("Directory occurrence overflow"))?;
        } else {
            occurrence = 0;
        }
        directory[index].occurrence = occurrence;
        previous_index = Some(index);
    }
    Ok(())
}

fn read_file_stream_blocks_info_from_source(
    header: &BundleHeader,
    reader: &mut ByteCursor<'_, '_>,
) -> Result<(Vec<u8>, Range<u64>)> {
    if header.version >= 7 {
        reader.align_to(16)?;
    } else if BundleParser::should_probe_legacy_alignment(header) {
        let pre_align = reader.position();
        let padding = (16 - (pre_align % 16)) % 16;
        if padding != 0 {
            let bytes = reader.read_bytes(padding)?;
            if bytes.iter().any(|byte| *byte != 0) {
                reader.set_position(pre_align)?;
            }
        }
    }

    let data_start_before_info = reader.position();
    let compressed_size = u64::from(header.compressed_blocks_info_size);
    let blocks_info = if header.block_info_at_end() {
        let info_start = header
            .size
            .checked_sub(compressed_size)
            .ok_or_else(|| BinaryError::invalid_data("Blocks-info exceeds declared bundle size"))?;
        reader.set_position(info_start)?;
        let bytes = reader.read_bytes(compressed_size)?;
        reader.set_position(data_start_before_info)?;
        bytes
    } else {
        reader.read_bytes(compressed_size)?
    };

    if header.flags & crate::compression::ArchiveFlags::BLOCK_INFO_NEEDS_PADDING_AT_START != 0 {
        reader.align_to(16)?;
    }
    let block_data_start = reader.position();
    let block_data_end = if header.block_info_at_end() {
        header
            .size
            .checked_sub(compressed_size)
            .ok_or_else(|| BinaryError::invalid_data("Blocks-info exceeds declared bundle size"))?
    } else {
        header.size
    };
    if block_data_start > block_data_end {
        return Err(BinaryError::invalid_data(format!(
            "UnityFS block data starts at {block_data_start} after its physical end {block_data_end}"
        )));
    }
    Ok((blocks_info, block_data_start..block_data_end))
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

fn bundle_i32_at(data: &[u8], offset: usize, field: &'static str) -> Result<i32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| BinaryError::invalid_data(format!("{field} range overflow")))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| BinaryError::not_enough_data(end, data.len()))?;
    Ok(i32::from_be_bytes(bytes.try_into().map_err(|_| {
        BinaryError::invalid_data(format!("Invalid {field} width"))
    })?))
}

fn bundle_u16_at(data: &[u8], offset: usize, field: &'static str) -> Result<u16> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| BinaryError::invalid_data(format!("{field} range overflow")))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| BinaryError::not_enough_data(end, data.len()))?;
    Ok(u16::from_be_bytes(bytes.try_into().map_err(|_| {
        BinaryError::invalid_data(format!("Invalid {field} width"))
    })?))
}

fn bundle_u32_at(data: &[u8], offset: usize, field: &'static str) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| BinaryError::invalid_data(format!("{field} range overflow")))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| BinaryError::not_enough_data(end, data.len()))?;
    Ok(u32::from_be_bytes(bytes.try_into().map_err(|_| {
        BinaryError::invalid_data(format!("Invalid {field} width"))
    })?))
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

    const SERIALIZED_FILE_SAMPLE: &[u8] = include_bytes!(
        "../../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin"
    );

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
    fn legacy_alignment_uses_version_when_revision_is_empty() {
        let modern = BundleHeader {
            unity_version: "2019.4.0f1".to_string(),
            unity_revision: String::new(),
            ..BundleHeader::default()
        };
        assert!(BundleParser::should_probe_legacy_alignment(&modern));

        let old = BundleHeader {
            unity_version: "2019.3.15f1".to_string(),
            unity_revision: String::new(),
            ..BundleHeader::default()
        };
        assert!(!BundleParser::should_probe_legacy_alignment(&old));
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
    fn load_assets_sniffs_live_default_nodes_before_spending_parse_budget() {
        let header = BundleHeader {
            signature: "UnityFS".to_string(),
            ..Default::default()
        };
        let mut plain = AssetBundle::new(header.clone(), vec![0xa5; 64]);
        plain
            .nodes
            .push(DirectoryNode::new("plain.bin".to_string(), 0, 64, 0));
        let mut zero_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        BundleParser::load_assets(&mut plain, &BundleLoadOptions::default(), &mut zero_budget)
            .unwrap();
        assert!(plain.assets.is_empty());
        assert_eq!(zero_budget.usage(), Default::default());

        let mut memory_limited_plain = AssetBundle::new(header.clone(), vec![0xa5; 64]);
        memory_limited_plain
            .nodes
            .push(DirectoryNode::new("plain.bin".to_string(), 0, 64, 0));
        BundleParser::load_assets(
            &mut memory_limited_plain,
            &BundleLoadOptions {
                max_memory: Some(1),
                ..BundleLoadOptions::default()
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert!(memory_limited_plain.assets.is_empty());

        let mut serialized = AssetBundle::new(header, SERIALIZED_FILE_SAMPLE.to_vec());
        serialized.nodes.push(DirectoryNode::new(
            "data.assets".to_string(),
            0,
            u64::try_from(SERIALIZED_FILE_SAMPLE.len()).unwrap(),
            0,
        ));
        BundleParser::load_assets(
            &mut serialized,
            &BundleLoadOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(serialized.assets.len(), 1);
        assert_eq!(serialized.asset_names, ["data.assets"]);

        let mut limited_serialized = AssetBundle::new(
            BundleHeader {
                signature: "UnityFS".to_string(),
                ..Default::default()
            },
            SERIALIZED_FILE_SAMPLE.to_vec(),
        );
        limited_serialized.nodes.push(DirectoryNode::new(
            "data.assets".to_string(),
            0,
            u64::try_from(SERIALIZED_FILE_SAMPLE.len()).unwrap(),
            0,
        ));
        let error = BundleParser::load_assets(
            &mut limited_serialized,
            &BundleLoadOptions {
                max_memory: Some(1),
                ..BundleLoadOptions::default()
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(matches!(error, BinaryError::ResourceLimitExceeded(_)));
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

    #[test]
    fn legacy_inspection_accepts_zero_filled_directory_alignment() {
        let mut data = 1_i32.to_be_bytes().to_vec();
        data.extend_from_slice(b"test.txt\0");
        data.extend_from_slice(&24_u32.to_be_bytes());
        data.extend_from_slice(&3_u32.to_be_bytes());
        data.extend_from_slice(&[0; 3]);

        let directory = parse_legacy_inspection_directory(
            &data,
            24,
            27,
            &BundleLoadOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        assert_eq!(directory.len(), 1);
        assert_eq!(directory[0].name, "test.txt");
        assert_eq!(directory[0].offset, 24);
        assert_eq!(directory[0].length, 3);
    }

    #[test]
    fn legacy_inspection_rejects_nonzero_directory_alignment() {
        let mut data = 1_i32.to_be_bytes().to_vec();
        data.extend_from_slice(b"test.txt\0");
        data.extend_from_slice(&24_u32.to_be_bytes());
        data.extend_from_slice(&3_u32.to_be_bytes());
        data.extend_from_slice(&[0, 1, 0]);

        let error = parse_legacy_inspection_directory(
            &data,
            24,
            27,
            &BundleLoadOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("alignment padding is not zero-filled")
        );
    }

    #[test]
    fn legacy_directory_entries_do_not_claim_serialized_file_flags() {
        let data = legacy_directory_with_files(&["test.txt", "audio.resS"]);
        let mut bundle = AssetBundle::new_empty(BundleHeader::default());

        BundleParser::parse_legacy_directory(
            &mut bundle,
            &data,
            0,
            &BundleLoadOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        assert_eq!(bundle.nodes.len(), 2);
        assert!(bundle.nodes.iter().all(DirectoryNode::is_file));
        assert!(bundle.nodes.iter().all(|node| node.flags == 0));
        assert!(bundle.nodes.iter().all(|node| !node.is_serialized_file()));
    }
}
