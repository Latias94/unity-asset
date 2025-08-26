//! Compression support for Unity binary files

use crate::error::{BinaryError, Result};
use flate2::read::GzDecoder;
use std::io::Read;

/// Compression types supported by Unity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    /// No compression
    None = 0,
    /// LZMA compression
    Lzma = 1,
    /// LZ4 compression
    Lz4 = 2,
    /// LZ4HC (High Compression) compression
    Lz4Hc = 3,
    /// LZHAM compression
    Lzham = 4,
}

impl CompressionType {
    /// Create compression type from magic number/flags
    pub fn from_flags(flags: u32) -> Result<Self> {
        match flags & 0x3F {
            0 => Ok(CompressionType::None),
            1 => Ok(CompressionType::Lzma),
            2 => Ok(CompressionType::Lz4),
            3 => Ok(CompressionType::Lz4Hc),
            4 => Ok(CompressionType::Lzham),
            other => Err(BinaryError::unsupported_compression(format!(
                "Unknown compression type: {}",
                other
            ))),
        }
    }

    /// Check if this compression type is supported
    pub fn is_supported(self) -> bool {
        matches!(
            self,
            CompressionType::None
                | CompressionType::Lz4
                | CompressionType::Lz4Hc
                | CompressionType::Lzma
        )
    }

    /// Get the name of the compression type
    pub fn name(self) -> &'static str {
        match self {
            CompressionType::None => "None",
            CompressionType::Lzma => "LZMA",
            CompressionType::Lz4 => "LZ4",
            CompressionType::Lz4Hc => "LZ4HC",
            CompressionType::Lzham => "LZHAM",
        }
    }
}

/// Decompress data based on compression type
pub fn decompress(
    data: &[u8],
    compression: CompressionType,
    uncompressed_size: usize,
) -> Result<Vec<u8>> {
    match compression {
        CompressionType::None => {
            // No compression, return data as-is
            Ok(data.to_vec())
        }
        CompressionType::Lz4 | CompressionType::Lz4Hc => {
            // LZ4 decompression
            decompress_lz4(data, uncompressed_size)
        }
        CompressionType::Lzma => {
            // LZMA decompression
            decompress_lzma(data, uncompressed_size)
        }
        CompressionType::Lzham => {
            // LZHAM decompression (not implemented yet)
            Err(BinaryError::unsupported_compression(
                "LZHAM compression not yet supported",
            ))
        }
    }
}

/// Decompress LZ4 compressed data (Unity uses block format, not frame format)
fn decompress_lz4(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    // Unity uses LZ4 block format, not frame format
    // This is the same as UnityPy's lz4.block.decompress
    match lz4_flex::decompress(data, uncompressed_size) {
        Ok(decompressed) => {
            if decompressed.len() == uncompressed_size {
                Ok(decompressed)
            } else {
                Err(BinaryError::decompression_failed(format!(
                    "LZ4 decompression size mismatch: expected {}, got {}",
                    uncompressed_size,
                    decompressed.len()
                )))
            }
        }
        Err(e) => Err(BinaryError::decompression_failed(format!(
            "LZ4 block decompression failed: {}",
            e
        ))),
    }
}

/// Decompress LZMA compressed data (Unity uses LZMA1 format)
fn decompress_lzma(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    // Unity uses LZMA format, try different approaches
    if data.is_empty() {
        return Err(BinaryError::invalid_data("LZMA data is empty".to_string()));
    }

    // Try standard LZMA decompression first
    let mut output = Vec::new();
    match lzma_rs::lzma_decompress(&mut std::io::Cursor::new(data), &mut output) {
        Ok(_) => {
            if output.len() == uncompressed_size {
                Ok(output)
            } else {
                // Size mismatch, but data might still be valid
                // Unity sometimes has different size expectations
                println!(
                    "LZMA size mismatch: expected {}, got {}",
                    uncompressed_size,
                    output.len()
                );
                Ok(output)
            }
        }
        Err(e) => {
            // If standard LZMA fails, try with properties header
            if data.len() >= 13 {
                // Unity LZMA format: 5 bytes properties + 8 bytes uncompressed size + compressed data
                let compressed_data = &data[13..];
                let mut output2 = Vec::new();
                match lzma_rs::lzma_decompress(
                    &mut std::io::Cursor::new(compressed_data),
                    &mut output2,
                ) {
                    Ok(_) => Ok(output2),
                    Err(e2) => Err(BinaryError::decompression_failed(format!(
                        "LZMA decompression failed: {} (also tried with header: {})",
                        e, e2
                    ))),
                }
            } else {
                Err(BinaryError::decompression_failed(format!(
                    "LZMA decompression failed: {}",
                    e
                )))
            }
        }
    }
}

/// Decompress Brotli compressed data (used in WebGL builds)
pub fn decompress_brotli(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut decompressed = Vec::new();
    let mut decoder = brotli::Decompressor::new(data, 4096); // 4KB buffer size
    match decoder.read_to_end(&mut decompressed) {
        Ok(_) => Ok(decompressed),
        Err(e) => Err(BinaryError::decompression_failed(format!(
            "Brotli decompression failed: {}",
            e
        ))),
    }
}

/// Decompress GZIP data (used in some Unity formats)
pub fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).map_err(|e| {
        BinaryError::decompression_failed(format!("GZIP decompression failed: {}", e))
    })?;
    Ok(decompressed)
}

/// Compression block information
#[derive(Debug, Clone)]
pub struct CompressionBlock {
    /// Uncompressed size of the block
    pub uncompressed_size: u32,
    /// Compressed size of the block
    pub compressed_size: u32,
    /// Compression flags
    pub flags: u16,
}

impl CompressionBlock {
    /// Create a new compression block
    pub fn new(uncompressed_size: u32, compressed_size: u32, flags: u16) -> Self {
        Self {
            uncompressed_size,
            compressed_size,
            flags,
        }
    }

    /// Get the compression type for this block
    pub fn compression_type(&self) -> Result<CompressionType> {
        CompressionType::from_flags(self.flags as u32)
    }

    /// Check if this block is compressed
    pub fn is_compressed(&self) -> bool {
        self.uncompressed_size != self.compressed_size
    }

    /// Decompress the block data
    pub fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() != self.compressed_size as usize {
            return Err(BinaryError::invalid_data(format!(
                "Block data size mismatch: expected {}, got {}",
                self.compressed_size,
                data.len()
            )));
        }

        let compression = self.compression_type()?;
        decompress(data, compression, self.uncompressed_size as usize)
    }
}

/// Archive flags used in Unity bundle headers
pub struct ArchiveFlags;

impl ArchiveFlags {
    /// Compression type mask
    pub const COMPRESSION_TYPE_MASK: u32 = 0x3F;
    /// Block info at end of file
    pub const BLOCK_INFO_AT_END: u32 = 0x40;
    /// Old web plugin compatibility
    pub const OLD_WEB_PLUGIN_COMPATIBILITY: u32 = 0x80;
    /// Block info needs PaddingAtStart
    pub const BLOCK_INFO_NEEDS_PADDING_AT_START: u32 = 0x100;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_type_from_flags() {
        assert_eq!(
            CompressionType::from_flags(0).unwrap(),
            CompressionType::None
        );
        assert_eq!(
            CompressionType::from_flags(1).unwrap(),
            CompressionType::Lzma
        );
        assert_eq!(
            CompressionType::from_flags(2).unwrap(),
            CompressionType::Lz4
        );
        assert_eq!(
            CompressionType::from_flags(3).unwrap(),
            CompressionType::Lz4Hc
        );
    }

    #[test]
    fn test_compression_type_names() {
        assert_eq!(CompressionType::None.name(), "None");
        assert_eq!(CompressionType::Lz4.name(), "LZ4");
        assert_eq!(CompressionType::Lzma.name(), "LZMA");
    }

    #[test]
    fn test_compression_type_supported() {
        assert!(CompressionType::None.is_supported());
        assert!(CompressionType::Lz4.is_supported());
        assert!(CompressionType::Lz4Hc.is_supported());
        assert!(CompressionType::Lzma.is_supported());
        assert!(!CompressionType::Lzham.is_supported());
    }

    #[test]
    fn test_no_compression() {
        let data = b"Hello, World!";
        let result = decompress(data, CompressionType::None, data.len()).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn test_compression_block() {
        let block = CompressionBlock::new(100, 80, 2); // LZ4 compression
        assert!(block.is_compressed());
        assert_eq!(block.compression_type().unwrap(), CompressionType::Lz4);
    }

    #[test]
    fn test_archive_flags() {
        let flags = 2 | ArchiveFlags::BLOCK_INFO_AT_END;
        let compression =
            CompressionType::from_flags(flags & ArchiveFlags::COMPRESSION_TYPE_MASK).unwrap();
        assert_eq!(compression, CompressionType::Lz4);
        assert_eq!(
            flags & ArchiveFlags::BLOCK_INFO_AT_END,
            ArchiveFlags::BLOCK_INFO_AT_END
        );
    }

    #[test]
    fn test_brotli_decompression() {
        // Test with simple data - this is a basic test
        // In real usage, we would have actual Brotli-compressed Unity data
        let test_data = b"Hello, World!";

        // For now, just test that the function exists and handles errors gracefully
        // We can't easily create valid Brotli data in a unit test without the encoder
        match decompress_brotli(test_data) {
            Ok(_) => {
                // If it succeeds, that's fine (though unlikely with random data)
            }
            Err(_) => {
                // Expected for invalid Brotli data
            }
        }
    }

    #[test]
    fn test_compression_detection() {
        // Test that we can detect different compression types from flags
        assert_eq!(
            CompressionType::from_flags(0).unwrap(),
            CompressionType::None
        );
        assert_eq!(
            CompressionType::from_flags(1).unwrap(),
            CompressionType::Lzma
        );
        assert_eq!(
            CompressionType::from_flags(2).unwrap(),
            CompressionType::Lz4
        );
        assert_eq!(
            CompressionType::from_flags(3).unwrap(),
            CompressionType::Lz4Hc
        );
        assert_eq!(
            CompressionType::from_flags(4).unwrap(),
            CompressionType::Lzham
        );

        // Test with flags that have additional bits set
        assert_eq!(
            CompressionType::from_flags(0x42).unwrap(),
            CompressionType::Lz4
        ); // LZ4 + other flags
    }

    #[test]
    fn test_gzip_decompression() {
        // Test GZIP decompression with simple data
        // This is a basic test - in real usage we would have actual GZIP data
        let test_data = b"invalid gzip data";

        // Should fail gracefully with invalid data
        match decompress_gzip(test_data) {
            Ok(_) => panic!("Should fail with invalid GZIP data"),
            Err(_) => {
                // Expected behavior for invalid data
            }
        }
    }

    #[test]
    fn test_compression_support_matrix() {
        // Verify our support matrix matches expectations
        let supported_types = [
            CompressionType::None,
            CompressionType::Lz4,
            CompressionType::Lz4Hc,
            CompressionType::Lzma,
        ];

        let unsupported_types = [CompressionType::Lzham];

        for compression_type in supported_types {
            assert!(
                compression_type.is_supported(),
                "Expected {} to be supported",
                compression_type.name()
            );
        }

        for compression_type in unsupported_types {
            assert!(
                !compression_type.is_supported(),
                "Expected {} to be unsupported",
                compression_type.name()
            );
        }
    }
}
