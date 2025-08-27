//! Bundle compression handling
//!
//! This module provides compression and decompression functionality
//! for Unity AssetBundle blocks, supporting LZ4, LZMA, and Brotli.

use super::header::BundleHeader;
use crate::compression::{CompressionBlock, CompressionType, decompress};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};

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
        let compression_type = header.flags & 0x3F; // CompressionTypeMask

        match compression_type {
            0 => {
                // No compression
                Ok(compressed_data.to_vec())
            }
            2 | 3 => {
                // LZ4 or LZ4HC
                decompress(
                    compressed_data,
                    CompressionType::Lz4,
                    header.uncompressed_blocks_info_size as usize,
                )
            }
            1 => {
                // LZMA
                decompress(
                    compressed_data,
                    CompressionType::Lzma,
                    header.uncompressed_blocks_info_size as usize,
                )
            }
            4 => {
                // Brotli (newer Unity versions)
                #[cfg(feature = "brotli")]
                {
                    decompress(
                        compressed_data,
                        CompressionType::Brotli,
                        header.uncompressed_blocks_info_size as usize,
                    )
                }
                #[cfg(not(feature = "brotli"))]
                {
                    Err(BinaryError::unsupported(
                        "Brotli compression requires brotli feature",
                    ))
                }
            }
            _ => Err(BinaryError::unsupported(format!(
                "Unknown compression type: {}",
                compression_type
            ))),
        }
    }

    /// Parse compression blocks from decompressed blocks info
    ///
    /// This method parses the compression block metadata from the
    /// decompressed blocks info data.
    pub fn parse_compression_blocks(data: &[u8]) -> Result<Vec<CompressionBlock>> {
        let mut reader = BinaryReader::new(data, ByteOrder::Big);
        let mut blocks = Vec::new();

        // Skip uncompressed data hash (16 bytes) - critical step
        reader.read_bytes(16)?;

        // Read compression blocks
        let block_count = reader.read_i32()? as usize;

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
        let mut decompressed_data = Vec::new();

        // Calculate the position where block data starts
        // TEMPORARY FIX: Always assume blocks info is after header, ignore the flag
        // This matches our fix in read_blocks_info
        let mut data_pos = header.header_size() + header.compressed_blocks_info_size as u64;

        // Process each compression block
        for block in blocks.iter() {
            reader.set_position(data_pos)?;
            let compressed_data = reader.read_bytes(block.compressed_size as usize)?;

            // Check if we have enough data
            if compressed_data.len() != block.compressed_size as usize {
                return Err(BinaryError::not_enough_data(
                    block.compressed_size as usize,
                    compressed_data.len(),
                ));
            }

            let block_data = block.decompress(&compressed_data)?;
            decompressed_data.extend_from_slice(&block_data);

            // Move to next block position
            data_pos += block.compressed_size as u64;
        }

        Ok(decompressed_data)
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
            if block.compressed_size > block.uncompressed_size * 2 && block.uncompressed_size > 1024
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
        // Estimate peak memory usage during decompression
        let total_uncompressed: usize = blocks.iter().map(|b| b.uncompressed_size as usize).sum();
        let max_block_size: usize = blocks
            .iter()
            .map(|b| b.uncompressed_size as usize)
            .max()
            .unwrap_or(0);

        // Peak usage: total output + largest single block for temporary decompression
        total_uncompressed + max_block_size
    }

    /// Check if compression type is supported
    pub fn is_compression_supported(compression_type: u32) -> bool {
        match compression_type {
            0 => true,     // None
            1 => true,     // LZMA
            2 | 3 => true, // LZ4/LZ4HC
            #[cfg(feature = "brotli")]
            4 => true, // Brotli
            #[cfg(not(feature = "brotli"))]
            4 => false, // Brotli
            _ => false,
        }
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

    #[test]
    fn test_compression_support() {
        assert!(BundleCompression::is_compression_supported(0)); // None
        assert!(BundleCompression::is_compression_supported(1)); // LZMA
        assert!(BundleCompression::is_compression_supported(2)); // LZ4
        assert!(BundleCompression::is_compression_supported(3)); // LZ4HC
        assert!(!BundleCompression::is_compression_supported(99)); // Unknown
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
}
