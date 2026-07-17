//! Bundle compression handling
//!
//! This module provides compression and decompression functionality
//! for Unity AssetBundle blocks, supporting LZ4, LZMA, and Brotli.

use super::header::BundleHeader;
use crate::compression::{
    CompressionBlock, CompressionType, decompress_with_budget, decompressor_scratch_bytes,
};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use unity_asset_core::AssetLoadBudget;

/// Bundle compression handler
///
/// This struct provides methods for handling compressed bundle data,
/// including block info decompression and data block processing.
pub struct BundleCompression;

impl BundleCompression {
    /// Decompress blocks info data
    ///
    /// This method handles the decompression of the blocks information
    /// section of a bundle, which contains metadata about all compression blocks.
    pub fn decompress_blocks_info(
        header: &BundleHeader,
        compressed_data: &[u8],
    ) -> Result<Vec<u8>> {
        let mut budget = AssetLoadBudget::default();
        Self::decompress_blocks_info_with_budget(header, compressed_data, &mut budget)
    }

    pub fn decompress_blocks_info_with_budget(
        header: &BundleHeader,
        compressed_data: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<u8>> {
        Self::decompress_blocks_info_limited_with_budget(header, compressed_data, None, budget)
    }

    pub fn decompress_blocks_info_limited(
        header: &BundleHeader,
        compressed_data: &[u8],
        max_uncompressed_size: Option<usize>,
    ) -> Result<Vec<u8>> {
        let mut budget = AssetLoadBudget::default();
        Self::decompress_blocks_info_limited_with_budget(
            header,
            compressed_data,
            max_uncompressed_size,
            &mut budget,
        )
    }

    pub fn decompress_blocks_info_limited_with_budget(
        header: &BundleHeader,
        compressed_data: &[u8],
        max_uncompressed_size: Option<usize>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<u8>> {
        let expected_uncompressed = usize::try_from(header.uncompressed_blocks_info_size)
            .map_err(|_| BinaryError::invalid_data("Blocks info size does not fit in usize"))?;
        if let Some(limit) = max_uncompressed_size
            && expected_uncompressed > limit
        {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "Blocks info uncompressed size {} exceeds limit {}",
                expected_uncompressed, limit
            )));
        }
        let compression = CompressionType::from_flags(header.flags)?;
        if !compression.is_supported() {
            return Err(BinaryError::unsupported_compression(format!(
                "{} compression not yet supported",
                compression.name()
            )));
        }
        budget.check_decompression(
            u64::try_from(compressed_data.len()).map_err(|_| {
                BinaryError::invalid_data("Blocks info compressed size does not fit in u64")
            })?,
            u64::try_from(expected_uncompressed).map_err(|_| {
                BinaryError::invalid_data("Blocks info uncompressed size does not fit in u64")
            })?,
        )?;
        decompress_with_budget(compressed_data, compression, expected_uncompressed, budget)
    }

    /// Parse compression blocks from decompressed blocks info
    ///
    /// This method parses the compression block metadata from the
    /// decompressed blocks info data.
    pub fn parse_compression_blocks(data: &[u8]) -> Result<Vec<CompressionBlock>> {
        Self::parse_compression_blocks_limited(data, &super::types::BundleLoadOptions::lazy())
    }

    pub fn parse_compression_blocks_limited(
        data: &[u8],
        options: &super::types::BundleLoadOptions,
    ) -> Result<Vec<CompressionBlock>> {
        let mut budget = AssetLoadBudget::default();
        Self::parse_compression_blocks_limited_with_budget(data, options, &mut budget)
    }

    pub fn parse_compression_blocks_limited_with_budget(
        data: &[u8],
        options: &super::types::BundleLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<CompressionBlock>> {
        let mut reader = BinaryReader::new(data, ByteOrder::Big);

        // Skip uncompressed data hash (16 bytes) - critical step
        reader.read_bytes(16)?;

        // Read compression blocks
        let block_count_i32 = reader.read_i32()?;
        if block_count_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative compression block count: {}",
                block_count_i32
            )));
        }
        let block_count = usize::try_from(block_count_i32)
            .map_err(|_| BinaryError::invalid_data("Negative compression block count"))?;
        if block_count > options.max_blocks {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "Compression block count {} exceeds limit {}",
                block_count, options.max_blocks
            )));
        }

        // Ensure the block table fits in the provided buffer.
        let table_bytes = block_count
            .checked_mul(10)
            .ok_or_else(|| BinaryError::invalid_data("Compression block table size overflow"))?;
        let required = 16usize
            .checked_add(4)
            .and_then(|v| v.checked_add(table_bytes))
            .ok_or_else(|| BinaryError::invalid_data("Compression block table size overflow"))?;
        if data.len() < required {
            return Err(BinaryError::not_enough_data(required, data.len()));
        }
        budget.consume_entries(u64::try_from(block_count).map_err(|_| {
            BinaryError::invalid_data("Compression block count does not fit in u64")
        })?)?;
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(block_count).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {block_count} compression blocks: {error}"
            ))
        })?;

        for _ in 0..block_count {
            let uncompressed_size = reader.read_u32()?;
            let compressed_size = reader.read_u32()?;
            let flags = reader.read_u16()?;

            let block = CompressionBlock::new(uncompressed_size, compressed_size, flags);
            blocks.push(block);
        }

        Ok(blocks)
    }

    /// Decompress all data blocks
    ///
    /// This method reads and decompresses all data blocks from the bundle,
    /// returning the complete decompressed data.
    pub fn decompress_data_blocks(
        header: &BundleHeader,
        blocks: &[CompressionBlock],
        reader: &mut BinaryReader,
    ) -> Result<Vec<u8>> {
        let mut budget = AssetLoadBudget::default();
        Self::decompress_data_blocks_with_budget(header, blocks, reader, &mut budget)
    }

    pub fn decompress_data_blocks_with_budget(
        header: &BundleHeader,
        blocks: &[CompressionBlock],
        reader: &mut BinaryReader,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<u8>> {
        Self::decompress_data_blocks_limited_with_budget(header, blocks, reader, None, budget)
    }

    pub fn decompress_data_blocks_limited(
        header: &BundleHeader,
        blocks: &[CompressionBlock],
        reader: &mut BinaryReader,
        max_memory: Option<usize>,
    ) -> Result<Vec<u8>> {
        let mut budget = AssetLoadBudget::default();
        Self::decompress_data_blocks_limited_with_budget(
            header,
            blocks,
            reader,
            max_memory,
            &mut budget,
        )
    }

    pub fn decompress_data_blocks_limited_with_budget(
        _header: &BundleHeader,
        blocks: &[CompressionBlock],
        reader: &mut BinaryReader,
        max_memory: Option<usize>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<u8>> {
        let total_uncompressed = Self::preflight_data_blocks(blocks, reader, max_memory, budget)?;
        let mut decompressed_data = Vec::new();
        decompressed_data
            .try_reserve_exact(total_uncompressed)
            .map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {total_uncompressed} bundle output bytes: {error}"
                ))
            })?;

        for block in blocks.iter() {
            let compressed_size = usize::try_from(block.compressed_size).map_err(|_| {
                BinaryError::invalid_data("Block compressed size does not fit in usize")
            })?;
            let compressed = reader.read_bytes(compressed_size)?;
            let block_data = block.decompress_with_budget(&compressed, budget)?;
            decompressed_data.extend_from_slice(&block_data);
        }

        Ok(decompressed_data)
    }

    pub(crate) fn preflight_data_blocks(
        blocks: &[CompressionBlock],
        reader: &BinaryReader<'_>,
        max_memory: Option<usize>,
        budget: &AssetLoadBudget,
    ) -> Result<usize> {
        let mut total_compressed = 0u64;
        let mut total_uncompressed = 0u64;

        for block in blocks {
            let compressed = u64::from(block.compressed_size);
            let uncompressed = u64::from(block.uncompressed_size);
            total_compressed = total_compressed
                .checked_add(compressed)
                .ok_or_else(|| BinaryError::invalid_data("Total compressed size overflow"))?;
            total_uncompressed = total_uncompressed
                .checked_add(uncompressed)
                .ok_or_else(|| BinaryError::invalid_data("Total uncompressed size overflow"))?;

            budget.check_decompression(compressed, uncompressed)?;
        }

        budget.check_decompression(total_compressed, total_uncompressed)?;

        let compressed_len = usize::try_from(total_compressed).map_err(|_| {
            BinaryError::ResourceLimitExceeded(format!(
                "Bundle compressed size {total_compressed} does not fit in usize"
            ))
        })?;
        if compressed_len > reader.remaining() {
            return Err(BinaryError::not_enough_data(
                compressed_len,
                reader.remaining(),
            ));
        }

        let source = reader.remaining_slice();
        let mut source_offset = 0usize;
        let mut max_block_working_set = 0u64;
        let mut total_codec_scratch = 0u64;
        for block in blocks {
            let compressed_size = usize::try_from(block.compressed_size).map_err(|_| {
                BinaryError::invalid_data("Block compressed size does not fit usize")
            })?;
            let source_end = source_offset
                .checked_add(compressed_size)
                .ok_or_else(|| BinaryError::invalid_data("Block source range overflow"))?;
            let compressed = source
                .get(source_offset..source_end)
                .ok_or_else(|| BinaryError::not_enough_data(source_end, source.len()))?;
            source_offset = source_end;

            let uncompressed_size = usize::try_from(block.uncompressed_size).map_err(|_| {
                BinaryError::invalid_data("Block uncompressed size does not fit usize")
            })?;
            let scratch = decompressor_scratch_bytes(
                compressed,
                block.compression_type()?,
                uncompressed_size,
            )?;
            total_codec_scratch = total_codec_scratch.checked_add(scratch).ok_or_else(|| {
                BinaryError::ResourceLimitExceeded(
                    "Cumulative decompressor scratch size overflow".to_string(),
                )
            })?;
            let block_working_set = u64::from(block.compressed_size)
                .checked_add(u64::from(block.uncompressed_size))
                .and_then(|amount| amount.checked_add(scratch))
                .ok_or_else(|| {
                    BinaryError::ResourceLimitExceeded(
                        "Block decompression working-set size overflow".to_string(),
                    )
                })?;
            max_block_working_set = max_block_working_set.max(block_working_set);
        }
        budget.check_bytes(total_codec_scratch)?;

        let peak_memory = total_uncompressed
            .checked_add(max_block_working_set)
            .ok_or_else(|| {
                BinaryError::ResourceLimitExceeded(
                    "Bundle decompression peak-memory size overflow".to_string(),
                )
            })?;
        if let Some(limit) = max_memory {
            let limit = u64::try_from(limit).map_err(|_| {
                BinaryError::ResourceLimitExceeded("max_memory does not fit u64".to_string())
            })?;
            if peak_memory > limit {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Bundle decompression peak memory {peak_memory} exceeds max_memory {limit}"
                )));
            }
        }

        usize::try_from(total_uncompressed).map_err(|_| {
            BinaryError::ResourceLimitExceeded(format!(
                "Bundle decompressed size {total_uncompressed} does not fit in usize"
            ))
        })
    }

    /// Get compression statistics for blocks
    pub fn get_compression_stats(blocks: &[CompressionBlock]) -> CompressionStats {
        let total_compressed: u64 = blocks.iter().map(|b| b.compressed_size as u64).sum();
        let total_uncompressed: u64 = blocks.iter().map(|b| b.uncompressed_size as u64).sum();

        let compression_ratio = if total_uncompressed > 0 {
            total_compressed as f64 / total_uncompressed as f64
        } else {
            1.0
        };

        let space_saved = total_uncompressed.saturating_sub(total_compressed);

        CompressionStats {
            block_count: blocks.len(),
            total_compressed_size: total_compressed,
            total_uncompressed_size: total_uncompressed,
            compression_ratio,
            space_saved,
            average_block_size: if !blocks.is_empty() {
                total_uncompressed / blocks.len() as u64
            } else {
                0
            },
        }
    }

    /// Validate compression blocks
    pub fn validate_blocks(blocks: &[CompressionBlock]) -> Result<()> {
        if blocks.is_empty() {
            return Err(BinaryError::invalid_data("No compression blocks found"));
        }

        for (i, block) in blocks.iter().enumerate() {
            if block.compressed_size == 0 {
                return Err(BinaryError::invalid_data(format!(
                    "Block {} has zero compressed size",
                    i
                )));
            }

            if block.uncompressed_size == 0 {
                return Err(BinaryError::invalid_data(format!(
                    "Block {} has zero uncompressed size",
                    i
                )));
            }

            // Sanity check: compressed size shouldn't be much larger than uncompressed
            // (except for very small blocks or incompressible data)
            if u64::from(block.compressed_size) > u64::from(block.uncompressed_size) * 2
                && block.uncompressed_size > 1024
            {
                return Err(BinaryError::invalid_data(format!(
                    "Block {} has suspicious compression ratio: {}/{}",
                    i, block.compressed_size, block.uncompressed_size
                )));
            }
        }

        Ok(())
    }

    /// Estimate memory usage for decompression
    pub fn estimate_memory_usage(blocks: &[CompressionBlock]) -> usize {
        let total_uncompressed = blocks.iter().fold(0usize, |total, block| {
            total.saturating_add(block.uncompressed_size as usize)
        });
        let max_block_working_set = blocks
            .iter()
            .map(|block| {
                (block.compressed_size as usize).saturating_add(block.uncompressed_size as usize)
            })
            .max()
            .unwrap_or(0);

        total_uncompressed.saturating_add(max_block_working_set)
    }

    /// Check if compression type is supported
    pub fn is_compression_supported(compression_type: u32) -> bool {
        CompressionType::from_flags(compression_type).is_ok_and(CompressionType::is_supported)
    }
}

/// Compression statistics
#[derive(Debug, Clone)]
pub struct CompressionStats {
    pub block_count: usize,
    pub total_compressed_size: u64,
    pub total_uncompressed_size: u64,
    pub compression_ratio: f64,
    pub space_saved: u64,
    pub average_block_size: u64,
}

impl CompressionStats {
    /// Get compression efficiency as a percentage
    pub fn efficiency_percent(&self) -> f64 {
        (1.0 - self.compression_ratio) * 100.0
    }

    /// Check if compression was effective
    pub fn is_effective(&self) -> bool {
        self.compression_ratio < 0.9 // Less than 90% of original size
    }
}

/// Compression options for bundle processing
#[derive(Debug, Clone)]
pub struct CompressionOptions {
    /// Maximum memory to use for decompression
    pub max_memory: Option<usize>,
    /// Whether to validate blocks before decompression
    pub validate_blocks: bool,
    /// Whether to collect compression statistics
    pub collect_stats: bool,
    /// Preferred compression type for new bundles
    pub preferred_compression: CompressionType,
}

impl Default for CompressionOptions {
    fn default() -> Self {
        Self {
            max_memory: Some(1024 * 1024 * 1024), // 1GB
            validate_blocks: true,
            collect_stats: false,
            preferred_compression: CompressionType::Lz4,
        }
    }
}

impl CompressionOptions {
    /// Create options for fast decompression (minimal validation)
    pub fn fast() -> Self {
        Self {
            max_memory: None,
            validate_blocks: false,
            collect_stats: false,
            preferred_compression: CompressionType::Lz4,
        }
    }

    /// Create options for safe decompression (full validation)
    pub fn safe() -> Self {
        Self {
            max_memory: Some(512 * 1024 * 1024), // 512MB
            validate_blocks: true,
            collect_stats: true,
            preferred_compression: CompressionType::Lz4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

    #[test]
    fn test_compression_support() {
        assert!(BundleCompression::is_compression_supported(0)); // None
        assert!(BundleCompression::is_compression_supported(1)); // LZMA
        assert!(BundleCompression::is_compression_supported(2)); // LZ4
        assert!(BundleCompression::is_compression_supported(3)); // LZ4HC
        assert!(!BundleCompression::is_compression_supported(4)); // LZHAM
        assert!(BundleCompression::is_compression_supported(5)); // Brotli
        assert!(!BundleCompression::is_compression_supported(99)); // Unknown
    }

    #[test]
    fn lzham_blocks_info_is_not_reinterpreted_as_brotli() {
        let header = BundleHeader {
            flags: CompressionType::Lzham as u32,
            compressed_blocks_info_size: 1,
            uncompressed_blocks_info_size: 1,
            ..Default::default()
        };
        let mut budget = AssetLoadBudget::default();

        let error =
            BundleCompression::decompress_blocks_info_with_budget(&header, &[0], &mut budget)
                .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::UnsupportedCompression(message) if message.contains("LZHAM")
        ));
        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn test_compression_stats() {
        let blocks = vec![
            CompressionBlock::new(1000, 500, 0),
            CompressionBlock::new(2000, 1000, 0),
        ];

        let stats = BundleCompression::get_compression_stats(&blocks);
        assert_eq!(stats.block_count, 2);
        assert_eq!(stats.total_compressed_size, 1500);
        assert_eq!(stats.total_uncompressed_size, 3000);
        assert_eq!(stats.compression_ratio, 0.5);
        assert_eq!(stats.space_saved, 1500);
        assert!(stats.is_effective());
    }

    #[test]
    fn data_blocks_share_one_cumulative_decompression_budget() {
        let header = BundleHeader::default();
        let blocks = vec![
            CompressionBlock::new(8, 8, 0),
            CompressionBlock::new(8, 8, 0),
        ];
        let bytes = [0x11_u8; 16];
        let mut reader = BinaryReader::new(&bytes, ByteOrder::Big);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_decompressed_bytes: 12,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = BundleCompression::decompress_data_blocks_with_budget(
            &header,
            &blocks,
            &mut reader,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "decompressed_bytes",
                limit: 12,
                requested: 16,
            })
        ));
        assert_eq!(reader.position(), 0);
        assert_eq!(budget.usage().compressed_bytes, 0);
        assert_eq!(budget.usage().decompressed_bytes, 0);
    }

    #[test]
    fn data_block_preflight_enforces_owned_peak_memory() {
        let blocks = vec![CompressionBlock::new(8, 8, CompressionType::None as u16)];
        let bytes = [0x11_u8; 8];
        let reader = BinaryReader::new(&bytes, ByteOrder::Big);
        let budget = AssetLoadBudget::default();

        let error = BundleCompression::preflight_data_blocks(&blocks, &reader, Some(23), &budget)
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::ResourceLimitExceeded(message)
                if message.contains("peak memory 24") && message.contains("max_memory 23")
        ));
        assert_eq!(reader.position(), 0);
        assert_eq!(budget.usage(), Default::default());

        assert_eq!(
            BundleCompression::preflight_data_blocks(&blocks, &reader, Some(24), &budget,).unwrap(),
            8
        );
    }

    #[test]
    fn block_validation_handles_u32_extremes_without_overflow() {
        let blocks = [CompressionBlock::new(u32::MAX, u32::MAX, 0)];
        BundleCompression::validate_blocks(&blocks).unwrap();
    }
}
