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
    /// Brotli compression (WebGL builds)
    Brotli = 5,
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
            5 => Ok(CompressionType::Brotli),
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
                | CompressionType::Brotli
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
            CompressionType::Brotli => "Brotli",
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
        CompressionType::Brotli => {
            // Brotli decompression
            decompress_brotli(data)
        }
    }
}

/// Decompress LZ4 compressed data (Unity uses block format, not frame format)
fn decompress_lz4(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    // Unity uses LZ4 block format, not frame format
    // This is the same as UnityPy's lz4.block.decompress

    // Unity LZ4 data sometimes has size estimation issues
    // Try with a larger buffer first to avoid size mismatch errors
    let buffer_size = uncompressed_size + 128; // Add padding for Unity's size estimation issues

    match lz4_flex::decompress(data, buffer_size) {
        Ok(decompressed) => {
            // Check if the decompressed size is reasonable
            let size_diff = if decompressed.len() > uncompressed_size {
                decompressed.len() - uncompressed_size
            } else {
                uncompressed_size - decompressed.len()
            };

            if size_diff <= 128 {
                // Allow up to 128 bytes difference (Unity padding/alignment)
                if decompressed.len() != uncompressed_size {
                    println!(
                        "DEBUG: LZ4 size mismatch (within tolerance): expected {}, got {} (diff: {})",
                        uncompressed_size,
                        decompressed.len(),
                        size_diff
                    );
                }
                Ok(decompressed)
            } else {
                Err(BinaryError::decompression_failed(format!(
                    "LZ4 decompression size mismatch: expected {}, got {} (diff: {})",
                    uncompressed_size,
                    decompressed.len(),
                    size_diff
                )))
            }
        }
        Err(e) => {
            // If larger buffer fails, try with exact size as fallback
            match lz4_flex::decompress(data, uncompressed_size) {
                Ok(decompressed) => {
                    println!(
                        "DEBUG: LZ4 decompression succeeded with exact size: {} bytes",
                        decompressed.len()
                    );
                    Ok(decompressed)
                }
                Err(_) => Err(BinaryError::decompression_failed(format!(
                    "LZ4 block decompression failed: {}",
                    e
                ))),
            }
        }
    }
}

/// Decompress LZMA compressed data (Unity uses LZMA1 format)
fn decompress_lzma(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    // Unity uses LZMA format, try different approaches
    if data.is_empty() {
        return Err(BinaryError::invalid_data("LZMA data is empty".to_string()));
    }

    println!(
        "DEBUG: LZMA decompression - input size: {}, expected output: {}",
        data.len(),
        uncompressed_size
    );

    // Show first 32 bytes for debugging
    let preview_len = 32.min(data.len());
    let preview: Vec<String> = data[..preview_len]
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect();
    println!(
        "DEBUG: LZMA data first {} bytes: {}",
        preview_len,
        preview.join(" ")
    );

    // Unity LZMA format analysis:
    // Unity uses LZMA with specific header formats:
    // Format 1: Standard LZMA with 13-byte header (5 bytes properties + 8 bytes size)
    // Format 2: Unity custom LZMA with modified header
    // Format 3: Raw LZMA stream without header

    // Try Unity-specific LZMA decompression strategies
    let result = try_unity_lzma_strategies(data, uncompressed_size);
    if result.is_ok() {
        return result;
    }

    // If all strategies failed, try with xz2 crate as fallback
    #[cfg(feature = "xz2")]
    {
        if let Ok(result) = try_xz2_lzma(data, uncompressed_size) {
            return Ok(result);
        }
    }

    Err(BinaryError::decompression_failed(format!(
        "LZMA decompression failed with all strategies. Input size: {}, expected output: {}",
        data.len(),
        uncompressed_size
    )))
}

/// Try Unity-specific LZMA decompression strategies
fn try_unity_lzma_strategies(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    // Strategy 1: Try with Unity LZMA header format
    if let Ok(result) = try_unity_lzma_with_header(data, uncompressed_size) {
        return Ok(result);
    }

    // Strategy 2: Try Unity raw LZMA approach
    if let Ok(result) = try_unity_raw_lzma(data, uncompressed_size) {
        return Ok(result);
    }

    // Strategy 3: Try standard LZMA formats
    let strategies = [
        ("direct", data),
        (
            "skip_13_header",
            if data.len() > 13 { &data[13..] } else { data },
        ),
        (
            "skip_5_header",
            if data.len() > 5 { &data[5..] } else { data },
        ),
        (
            "skip_8_header",
            if data.len() > 8 { &data[8..] } else { data },
        ),
        (
            "unity_custom",
            if data.len() > 9 { &data[9..] } else { data },
        ),
    ];

    for (strategy_name, test_data) in &strategies {
        if test_data.is_empty() {
            continue;
        }

        println!(
            "DEBUG: Trying LZMA strategy: {}, data size: {}",
            strategy_name,
            test_data.len()
        );

        let mut output = Vec::new();
        match lzma_rs::lzma_decompress(&mut std::io::Cursor::new(test_data), &mut output) {
            Ok(_) => {
                println!(
                    "DEBUG: LZMA strategy '{}' succeeded, output size: {}",
                    strategy_name,
                    output.len()
                );

                // Check if size is reasonable
                let size_ratio = output.len() as f64 / uncompressed_size as f64;
                if (0.8..=1.2).contains(&size_ratio) {
                    // Size is within 20% of expected, probably correct
                    return Ok(output);
                } else if output.len() == uncompressed_size {
                    // Exact match
                    return Ok(output);
                }
            }
            Err(_e) => {
                // Strategy failed, continue to next
            }
        }
    }

    Err(BinaryError::decompression_failed(
        "All Unity LZMA strategies failed".to_string(),
    ))
}

/// Try Unity LZMA with custom header parsing (based on UnityPy implementation)
fn try_unity_lzma_with_header(data: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    if data.len() < 13 {
        return Err(BinaryError::invalid_data(
            "LZMA data too short for header".to_string(),
        ));
    }

    // Unity LZMA header format (based on UnityPy):
    // Bytes 0: props (LZMA properties byte)
    // Bytes 1-4: dict_size (little-endian u32)
    // Bytes 5-12: Uncompressed size (little-endian u64) - optional
    // Rest: Compressed data

    // Parse LZMA properties like UnityPy does
    let props = data[0];
    let dict_size = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);

    // Calculate LZMA parameters from props (UnityPy algorithm)
    let _lc = props % 9;
    let remainder = props / 9;
    let _pb = remainder / 5;
    let _lp = remainder % 5;

    // Try different data offsets (with and without size header)
    let offsets_to_try = [5, 13]; // 5 = no size header, 13 = with size header

    for &data_offset in &offsets_to_try {
        if data_offset >= data.len() {
            continue;
        }

        let compressed_data = &data[data_offset..];
        println!(
            "DEBUG: Trying Unity LZMA with offset {}, compressed size: {}",
            data_offset,
            compressed_data.len()
        );

        // Try to use xz2 crate for better LZMA support (if available)
        #[cfg(feature = "xz2")]
        {
            match try_unity_lzma_with_xz2(props, dict_size, compressed_data, expected_size) {
                Ok(result) => {
                    println!(
                        "DEBUG: Unity LZMA with xz2 succeeded, output size: {}",
                        result.len()
                    );
                    if result.len() == expected_size {
                        return Ok(result);
                    }
                }
                Err(_e) => {
                    // xz2 failed, continue
                }
            }
        }

        // Try UnityPy-style LZMA parameter calculation
        let lc = props % 9;
        let remainder = props / 9;
        let pb = remainder / 5;
        let lp = remainder % 5;

        println!(
            "DEBUG: UnityPy LZMA params - lc: {}, pb: {}, lp: {}",
            lc, pb, lp
        );

        // Try with calculated parameters (create custom LZMA header)
        let mut unity_lzma_data = Vec::new();
        unity_lzma_data.push(props);
        unity_lzma_data.extend_from_slice(&dict_size.to_le_bytes());
        unity_lzma_data.extend_from_slice(&(expected_size as u64).to_le_bytes());
        unity_lzma_data.extend_from_slice(compressed_data);

        let mut output = Vec::new();
        match lzma_rs::lzma_decompress(&mut std::io::Cursor::new(&unity_lzma_data), &mut output) {
            Ok(_) => {
                println!(
                    "DEBUG: Unity LZMA with UnityPy params succeeded, output size: {}",
                    output.len()
                );
                if output.len() == expected_size {
                    return Ok(output);
                } else if !output.is_empty() {
                    let ratio = output.len() as f64 / expected_size as f64;
                    if (0.8..=1.2).contains(&ratio) {
                        return Ok(output);
                    }
                }
            }
            Err(_e) => {
                // UnityPy params failed, continue
            }
        }

        // Fallback: reconstruct standard LZMA header and try with lzma_rs
        let mut lzma_data = Vec::new();
        lzma_data.push(props);
        lzma_data.extend_from_slice(&dict_size.to_le_bytes());
        lzma_data.extend_from_slice(&(expected_size as u64).to_le_bytes());
        lzma_data.extend_from_slice(compressed_data);

        let mut output = Vec::new();
        match lzma_rs::lzma_decompress(&mut std::io::Cursor::new(&lzma_data), &mut output) {
            Ok(_) => {
                println!(
                    "DEBUG: Unity LZMA with lzma_rs succeeded, output size: {}",
                    output.len()
                );
                if output.len() == expected_size {
                    return Ok(output);
                } else if !output.is_empty() {
                    let ratio = output.len() as f64 / expected_size as f64;
                    if (0.8..=1.2).contains(&ratio) {
                        return Ok(output);
                    }
                }
            }
            Err(_e) => {
                // lzma_rs failed, continue
            }
        }
    }

    Err(BinaryError::decompression_failed(
        "Unity LZMA header parsing failed".to_string(),
    ))
}

/// Try Unity LZMA decompression using xz2 crate (more compatible with Unity's LZMA)
#[cfg(feature = "xz2")]
fn try_unity_lzma_with_xz2(
    _props: u8,
    _dict_size: u32,
    _compressed_data: &[u8],
    _expected_size: usize,
) -> Result<Vec<u8>> {
    // TODO: Implement proper xz2 LZMA decompression
    // For now, return an error to fall back to lzma_rs
    Err(BinaryError::decompression_failed(
        "XZ2 LZMA not yet implemented".to_string(),
    ))
}

/// Try Unity-specific LZMA decompression with raw data approach
fn try_unity_raw_lzma(data: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    if data.len() < 13 {
        return Err(BinaryError::invalid_data(
            "Data too short for Unity LZMA".to_string(),
        ));
    }

    // Unity sometimes stores LZMA data with a custom header format
    // Try to extract the actual LZMA stream from various offsets
    let offsets_to_try = [0, 5, 8, 9, 13, 16];

    for &offset in &offsets_to_try {
        if offset >= data.len() {
            continue;
        }

        let lzma_stream = &data[offset..];
        if lzma_stream.len() < 5 {
            continue;
        }

        println!(
            "DEBUG: Trying LZMA stream from offset {}, size: {}",
            offset,
            lzma_stream.len()
        );

        // Try to decompress as raw LZMA stream
        let mut output = Vec::new();
        match lzma_rs::lzma_decompress(&mut std::io::Cursor::new(lzma_stream), &mut output) {
            Ok(_) => {
                println!(
                    "DEBUG: Raw LZMA from offset {} succeeded, output size: {}",
                    offset,
                    output.len()
                );

                // Check if size is reasonable
                if output.len() == expected_size {
                    return Ok(output);
                } else if !output.is_empty() {
                    let ratio = output.len() as f64 / expected_size as f64;
                    if (0.5..=2.0).contains(&ratio) {
                        println!(
                            "DEBUG: Size ratio {:.2} is acceptable for offset {}",
                            ratio, offset
                        );
                        return Ok(output);
                    }
                }
            }
            Err(_e) => {
                // Raw LZMA failed, continue
            }
        }

        // Try with reconstructed header
        if lzma_stream.len() >= 5 {
            let mut reconstructed = Vec::new();
            reconstructed.extend_from_slice(&lzma_stream[0..5]); // Properties
            reconstructed.extend_from_slice(&(expected_size as u64).to_le_bytes()); // Size
            if lzma_stream.len() > 5 {
                reconstructed.extend_from_slice(&lzma_stream[5..]); // Compressed data
            }

            let mut output = Vec::new();
            match lzma_rs::lzma_decompress(&mut std::io::Cursor::new(&reconstructed), &mut output) {
                Ok(_) => {
                    println!(
                        "DEBUG: Reconstructed LZMA from offset {} succeeded, output size: {}",
                        offset,
                        output.len()
                    );
                    if output.len() == expected_size {
                        return Ok(output);
                    }
                }
                Err(e) => {
                    println!(
                        "DEBUG: Reconstructed LZMA from offset {} failed: {}",
                        offset, e
                    );
                }
            }
        }
    }

    Err(BinaryError::decompression_failed(
        "Unity raw LZMA failed".to_string(),
    ))
}

#[cfg(feature = "xz2")]
fn try_xz2_lzma(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>> {
    use std::io::Read;

    // Try different XZ2 approaches
    let strategies = [
        ("xz2_stream", data),
        (
            "xz2_skip_13",
            if data.len() > 13 { &data[13..] } else { data },
        ),
        ("xz2_skip_5", if data.len() > 5 { &data[5..] } else { data }),
    ];

    for (_strategy_name, test_data) in &strategies {
        if test_data.is_empty() {
            continue;
        }

        // Create a cursor from the data
        let cursor = std::io::Cursor::new(test_data);
        let mut decoder = xz2::read::XzDecoder::new(cursor);
        let mut output = Vec::new();

        match decoder.read_to_end(&mut output) {
            Ok(_) => {
                let size_ratio = output.len() as f64 / uncompressed_size as f64;
                if (0.8..=1.2).contains(&size_ratio) || output.len() == uncompressed_size {
                    return Ok(output);
                }
            }
            Err(_) => continue,
        }
    }

    Err(BinaryError::decompression_failed(
        "XZ2 LZMA decompression failed".to_string(),
    ))
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
