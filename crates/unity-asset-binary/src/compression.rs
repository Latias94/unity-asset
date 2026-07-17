//! Compression support for Unity binary files

use crate::error::{BinaryError, Result};
use flate2::bufread::GzDecoder;
use std::io::{BufRead, Read, Write};
use unity_asset_core::{AssetLoadBudget, DecompressionBudget};

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
    let mut budget = AssetLoadBudget::default();
    decompress_with_budget(data, compression, uncompressed_size, &mut budget)
}

/// Decompress data while charging one caller-owned load budget.
pub fn decompress_with_budget(
    data: &[u8],
    compression: CompressionType,
    uncompressed_size: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    if compression == CompressionType::Lzham {
        return Err(BinaryError::unsupported_compression(
            "LZHAM compression not yet supported",
        ));
    }
    budget.check_decompression(
        usize_to_u64(data.len(), "compressed input size")?,
        usize_to_u64(uncompressed_size, "declared uncompressed size")?,
    )?;

    match compression {
        CompressionType::None => copy_uncompressed_with_budget(data, uncompressed_size, budget),
        CompressionType::Lz4 | CompressionType::Lz4Hc => {
            decompress_lz4_with_budget(data, uncompressed_size, budget)
        }
        CompressionType::Lzma => decompress_lzma_with_budget(data, uncompressed_size, budget),
        CompressionType::Lzham => unreachable!("LZHAM is rejected before budget preflight"),
        CompressionType::Brotli => {
            decompress_brotli_exact_with_budget(data, uncompressed_size, budget)
        }
    }
}

fn copy_uncompressed_with_budget(
    data: &[u8],
    uncompressed_size: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    if data.len() != uncompressed_size {
        return Err(BinaryError::invalid_data(format!(
            "Uncompressed payload size mismatch: declared {uncompressed_size}, got {}",
            data.len()
        )));
    }
    let mut output = BudgetedOutput::new(budget, data.len(), Some(data.len()))?;
    output.append(data)?;
    output.finish()
}

/// Unity LZ4 blocks carry an exact decompressed size, so decode into caller-sized storage.
fn decompress_lz4_with_budget(
    data: &[u8],
    uncompressed_size: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    let mut output = BudgetedOutput::new(budget, data.len(), Some(uncompressed_size))?;
    output.reserve_declared(uncompressed_size)?;
    let written =
        lz4_flex::block::decompress_into(data, output.spare_output_mut()).map_err(|error| {
            BinaryError::decompression_failed(format!("LZ4 block decompression failed: {error}"))
        })?;
    if written != uncompressed_size {
        return Err(BinaryError::decompression_failed(format!(
            "LZ4 decompression size mismatch: expected {uncompressed_size}, got {written}"
        )));
    }
    output.commit_reserved(written)?;
    output.finish()
}

struct BudgetedOutput<'a> {
    budget: DecompressionBudget<'a>,
    bytes: Vec<u8>,
    maximum_output: Option<usize>,
    write_failure: Option<BinaryError>,
}

impl<'a> BudgetedOutput<'a> {
    fn new(
        load_budget: &'a mut AssetLoadBudget,
        compressed_size: usize,
        maximum_output: Option<usize>,
    ) -> Result<Self> {
        let mut budget = load_budget.begin_decompression();
        budget.consume(usize_to_u64(compressed_size, "compressed size")?, 0)?;
        Ok(Self {
            budget,
            bytes: Vec::new(),
            maximum_output,
            write_failure: None,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        let new_len = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| BinaryError::memory_error("decompressed output length overflow"))?;
        self.check_output_limit(new_len)?;
        self.budget
            .consume(0, usize_to_u64(bytes.len(), "decompressed chunk")?)?;
        self.bytes.try_reserve(bytes.len()).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {} decompressed bytes: {error}",
                bytes.len()
            ))
        })?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn consume_compressed(&mut self, bytes: usize) -> Result<()> {
        self.budget
            .consume(usize_to_u64(bytes, "compressed chunk")?, 0)?;
        Ok(())
    }

    fn reserve_declared(&mut self, size: usize) -> Result<()> {
        self.check_output_limit(size)?;
        self.budget
            .consume(0, usize_to_u64(size, "declared decompressed size")?)?;
        self.bytes.try_reserve_exact(size).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {size} decompressed bytes: {error}"
            ))
        })?;
        self.bytes.resize(size, 0);
        Ok(())
    }

    fn spare_output_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    fn commit_reserved(&mut self, written: usize) -> Result<()> {
        if written > self.bytes.len() {
            return Err(BinaryError::invalid_data(
                "decoder wrote beyond reserved output",
            ));
        }
        self.bytes.truncate(written);
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>> {
        if let Some(error) = self.write_failure {
            Err(error)
        } else {
            Ok(self.bytes)
        }
    }

    fn check_output_limit(&self, requested: usize) -> Result<()> {
        if let Some(limit) = self.maximum_output
            && requested > limit
        {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "decompressed output {requested} exceeds declared limit {limit}"
            )));
        }
        Ok(())
    }
}

impl Write for BudgetedOutput<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.write_failure.is_some() {
            return Err(std::io::Error::other("decompression output already failed"));
        }
        if let Err(error) = self.append(buffer) {
            let message = error.to_string();
            self.write_failure = Some(error);
            return Err(std::io::Error::other(message));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn usize_to_u64(value: usize, label: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BinaryError::invalid_data(format!("{label} does not fit in u64")))
}

fn validate_declared_size(codec: &str, output: Vec<u8>, expected_size: usize) -> Result<Vec<u8>> {
    if output.len() != expected_size {
        return Err(BinaryError::decompression_failed(format!(
            "{codec} decompression size mismatch: expected {expected_size}, got {}",
            output.len()
        )));
    }
    Ok(output)
}

/// Decompress LZMA compressed data (Unity uses LZMA1 format)
fn decompress_lzma_with_budget(
    data: &[u8],
    uncompressed_size: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    let (properties, dictionary_size) = parse_lzma_properties(data)?;
    let memory = lzma_memory_plan(properties, dictionary_size, uncompressed_size)?;
    let header = lzma_alone_header(properties, dictionary_size, uncompressed_size)?;
    let encoded = std::io::Cursor::new(header).chain(std::io::Cursor::new(&data[5..]));
    decode_lzma_stream(encoded, data.len(), uncompressed_size, memory, budget)
}

/// Decompress a legacy Unity LZMA stream whose 13-byte header embeds its output size.
pub(crate) fn decompress_lzma_size_stream_with_budget(
    data: &[u8],
    uncompressed_size: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    budget.check_decompression(
        usize_to_u64(data.len(), "compressed input size")?,
        usize_to_u64(uncompressed_size, "declared uncompressed size")?,
    )?;
    let (properties, dictionary_size) = parse_lzma_properties(data)?;
    let declared_size = data
        .get(5..13)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| {
            BinaryError::invalid_data("Unity LZMA size stream is shorter than its 13-byte header")
        })?;
    let expected = usize_to_u64(uncompressed_size, "LZMA expected size")?;
    if declared_size != expected && declared_size != u64::MAX {
        return Err(BinaryError::invalid_data(format!(
            "LZMA header declares {declared_size} bytes but the container declares {expected}"
        )));
    }
    let memory = lzma_memory_plan(properties, dictionary_size, uncompressed_size)?;
    decode_lzma_stream(
        std::io::Cursor::new(data),
        data.len(),
        uncompressed_size,
        memory,
        budget,
    )
}

fn parse_lzma_properties(data: &[u8]) -> Result<(u8, u32)> {
    let properties = *data.first().ok_or_else(|| {
        BinaryError::invalid_data("Unity LZMA payload is missing its properties header")
    })?;
    let dictionary_size = data
        .get(1..5)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| {
            BinaryError::invalid_data(
                "Unity LZMA payload is shorter than its 5-byte properties header",
            )
        })?;
    if properties >= 9 * 5 * 5 {
        return Err(BinaryError::invalid_data(format!(
            "Invalid Unity LZMA properties byte {properties}"
        )));
    }
    Ok((properties, dictionary_size))
}

const LZMA_MIN_DICTIONARY_SIZE: usize = 0x1000;
const LZMA_LITERAL_PROBABILITY_COLUMNS: usize = 0x300;
// Four position-slot trees, one alignment tree, and two length decoders in lzma-rs 0.3.0.
const LZMA_FIXED_PROBABILITY_BYTES: usize = 2_592;
const VEC_U8_MIN_HEAP_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy)]
struct LzmaMemoryPlan {
    dictionary_limit: usize,
    scratch_bytes: u64,
}

fn lzma_memory_plan(
    properties: u8,
    dictionary_size: u32,
    expected_size: usize,
) -> Result<LzmaMemoryPlan> {
    let dictionary_size = usize::try_from(dictionary_size)
        .map_err(|_| BinaryError::invalid_data("LZMA dictionary size does not fit usize"))?
        .max(LZMA_MIN_DICTIONARY_SIZE);
    let dictionary_limit = dictionary_size.min(expected_size);
    let dictionary_allocation_bytes = if dictionary_limit == 0 {
        0
    } else {
        dictionary_limit
            .max(VEC_U8_MIN_HEAP_CAPACITY)
            .checked_next_power_of_two()
            .ok_or_else(|| BinaryError::memory_error("LZMA dictionary capacity overflow"))?
    };

    // lzma-rs allocates this Vec2D eagerly in DecoderState::new.
    let literal_context_bits = usize::from(properties % 9)
        .checked_add(usize::from((properties / 9) % 5))
        .ok_or_else(|| BinaryError::memory_error("LZMA literal context size overflow"))?;
    let literal_contexts = 1_usize
        .checked_shl(u32::try_from(literal_context_bits).map_err(|_| {
            BinaryError::memory_error("LZMA literal context shift does not fit u32")
        })?)
        .ok_or_else(|| BinaryError::memory_error("LZMA literal context count overflow"))?;
    let literal_probability_bytes = literal_contexts
        .checked_mul(LZMA_LITERAL_PROBABILITY_COLUMNS)
        .and_then(|count| count.checked_mul(std::mem::size_of::<u16>()))
        .ok_or_else(|| BinaryError::memory_error("LZMA probability table size overflow"))?;
    let scratch_bytes = dictionary_allocation_bytes
        .checked_add(literal_probability_bytes)
        .and_then(|bytes| bytes.checked_add(LZMA_FIXED_PROBABILITY_BYTES))
        .ok_or_else(|| BinaryError::memory_error("LZMA decoder scratch size overflow"))?;

    Ok(LzmaMemoryPlan {
        dictionary_limit,
        scratch_bytes: usize_to_u64(scratch_bytes, "LZMA decoder scratch size")?,
    })
}

pub(crate) fn decompressor_scratch_bytes(
    data: &[u8],
    compression: CompressionType,
    uncompressed_size: usize,
) -> Result<u64> {
    match compression {
        CompressionType::Lzma => {
            let (properties, dictionary_size) = parse_lzma_properties(data)?;
            Ok(lzma_memory_plan(properties, dictionary_size, uncompressed_size)?.scratch_bytes)
        }
        CompressionType::Lzham => Err(BinaryError::unsupported_compression(
            "LZHAM compression not yet supported",
        )),
        CompressionType::None
        | CompressionType::Lz4
        | CompressionType::Lz4Hc
        | CompressionType::Brotli => Ok(0),
    }
}

fn lzma_alone_header(
    properties: u8,
    dictionary_size: u32,
    expected_size: usize,
) -> Result<[u8; 13]> {
    let mut header = [0_u8; 13];
    header[0] = properties;
    header[1..5].copy_from_slice(&dictionary_size.to_le_bytes());
    header[5..13]
        .copy_from_slice(&usize_to_u64(expected_size, "LZMA expected size")?.to_le_bytes());
    Ok(header)
}

fn decode_lzma_stream(
    mut encoded: impl BufRead,
    charged_compressed_size: usize,
    expected_size: usize,
    memory: LzmaMemoryPlan,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    budget.consume_bytes(memory.scratch_bytes)?;
    let mut output = BudgetedOutput::new(budget, charged_compressed_size, Some(expected_size))?;
    let options = lzma_rs::decompress::Options {
        memlimit: Some(memory.dictionary_limit),
        ..Default::default()
    };
    let decoded = lzma_rs::lzma_decompress_with_options(&mut encoded, &mut output, &options);
    let output = output.finish()?;
    decoded.map_err(|error| {
        BinaryError::decompression_failed(format!("LZMA decompression failed: {error}"))
    })?;
    validate_declared_size("LZMA", output, expected_size)
}

/// Decompress Brotli compressed data (used in WebGL builds)
pub fn decompress_brotli(data: &[u8]) -> Result<Vec<u8>> {
    let mut budget = AssetLoadBudget::default();
    decompress_brotli_with_budget(data, &mut budget)
}

/// Decompress Brotli data with bounded streaming output.
pub fn decompress_brotli_with_budget(data: &[u8], budget: &mut AssetLoadBudget) -> Result<Vec<u8>> {
    budget.check_compressed_bytes(usize_to_u64(data.len(), "Brotli input size")?)?;
    let decoder = brotli::Decompressor::new(data, 64 * 1024);
    decompress_reader_with_budget(decoder, "Brotli", data.len(), None, budget)
}

fn decompress_brotli_exact_with_budget(
    data: &[u8],
    expected_size: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    let decoder = brotli::Decompressor::new(data, 64 * 1024);
    let output =
        decompress_reader_with_budget(decoder, "Brotli", data.len(), Some(expected_size), budget)?;
    validate_declared_size("Brotli", output, expected_size)
}

/// Decompress GZIP data (used in some Unity formats)
pub fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut budget = AssetLoadBudget::default();
    decompress_gzip_with_budget(data, &mut budget)
}

/// Decompress GZIP data with bounded streaming output.
pub fn decompress_gzip_with_budget(data: &[u8], budget: &mut AssetLoadBudget) -> Result<Vec<u8>> {
    budget.check_compressed_bytes(usize_to_u64(data.len(), "GZIP input size")?)?;
    let mut decoder = GzDecoder::new(data);
    let mut remaining_input = decoder.get_ref().len();
    let initial_consumed = data.len().checked_sub(remaining_input).ok_or_else(|| {
        BinaryError::invalid_data("GZIP decoder reported an invalid input position")
    })?;
    let mut output = BudgetedOutput::new(budget, initial_consumed, None)?;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let decoded = decoder.read(&mut buffer);
        let next_remaining = decoder.get_ref().len();
        let consumed = remaining_input.checked_sub(next_remaining).ok_or_else(|| {
            BinaryError::invalid_data("GZIP decoder moved backwards in its input")
        })?;
        // Charge only bytes consumed by this member, before charging the output they produced.
        output.consume_compressed(consumed)?;
        remaining_input = next_remaining;

        let decoded = decoded.map_err(|error| {
            BinaryError::decompression_failed(format!("GZIP decompression failed: {error}"))
        })?;
        if decoded == 0 {
            break;
        }
        output.append(&buffer[..decoded])?;
    }

    if remaining_input != 0 {
        return Err(BinaryError::decompression_failed(format!(
            "GZIP stream contains {remaining_input} trailing bytes after its single member"
        )));
    }
    output.finish()
}

fn decompress_reader_with_budget(
    mut reader: impl Read,
    codec: &str,
    compressed_size: usize,
    maximum_output: Option<usize>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    let mut output = BudgetedOutput::new(budget, compressed_size, maximum_output)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                return output.finish().and_then(|_| {
                    Err(BinaryError::decompression_failed(format!(
                        "{codec} decompression failed: {error}"
                    )))
                });
            }
        };
        if read == 0 {
            break;
        }
        output.append(&buffer[..read])?;
    }
    output.finish()
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
        let mut budget = AssetLoadBudget::default();
        self.decompress_with_budget(data, &mut budget)
    }

    /// Decompress the block while charging a caller-owned load budget.
    pub fn decompress_with_budget(
        &self,
        data: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<u8>> {
        let compressed_size = usize::try_from(self.compressed_size)
            .map_err(|_| BinaryError::invalid_data("Block compressed size does not fit usize"))?;
        if data.len() != compressed_size {
            return Err(BinaryError::invalid_data(format!(
                "Block data size mismatch: expected {}, got {}",
                self.compressed_size,
                data.len()
            )));
        }

        let compression = self.compression_type()?;
        let uncompressed_size = usize::try_from(self.uncompressed_size)
            .map_err(|_| BinaryError::invalid_data("Block size does not fit usize"))?;
        decompress_with_budget(data, compression, uncompressed_size, budget)
    }
}

/// Archive flags used in Unity bundle headers
pub struct ArchiveFlags;

impl ArchiveFlags {
    /// Compression type mask
    pub const COMPRESSION_TYPE_MASK: u32 = 0x3F;
    /// Blocks and directory info combined (UnityFS)
    pub const BLOCKS_AND_DIRECTORY_INFO_COMBINED: u32 = 0x40;
    /// Block info at end of file (UnityFS)
    pub const BLOCK_INFO_AT_END: u32 = 0x80;
    /// Old web plugin compatibility
    pub const OLD_WEB_PLUGIN_COMPATIBILITY: u32 = 0x100;
    /// Block info needs PaddingAtStart
    pub const BLOCK_INFO_NEEDS_PADDING_AT_START: u32 = 0x200;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

    fn budget_with_decompression_limits(
        max_decompressed_bytes: u64,
        max_expansion_ratio: u32,
    ) -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_decompressed_bytes,
            max_expansion_ratio,
            ..AssetLoadLimits::default()
        })
        .unwrap()
    }

    fn gzip_compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli_compress(data: &[u8]) -> Vec<u8> {
        let mut compressed = Vec::new();
        {
            let mut encoder = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            encoder.write_all(data).unwrap();
        }
        compressed
    }

    fn lzma_compress_with_size(data: &[u8], unpacked_size: Option<u64>) -> Vec<u8> {
        let mut input = std::io::BufReader::new(std::io::Cursor::new(data));
        let mut compressed = Vec::new();
        let options = lzma_rs::compress::Options {
            unpacked_size: lzma_rs::compress::UnpackedSize::WriteToHeader(unpacked_size),
        };
        lzma_rs::lzma_compress_with_options(&mut input, &mut compressed, &options).unwrap();
        compressed
    }

    fn lzma_compress(data: &[u8]) -> Vec<u8> {
        lzma_compress_with_size(data, Some(u64::try_from(data.len()).unwrap()))
    }

    fn lzma_compress_unknown_size(data: &[u8]) -> Vec<u8> {
        lzma_compress_with_size(data, None)
    }

    fn unity_lzma_compress(data: &[u8]) -> Vec<u8> {
        let encoded = lzma_compress(data);
        let mut unity = Vec::with_capacity(encoded.len() - 8);
        unity.extend_from_slice(&encoded[..5]);
        unity.extend_from_slice(&encoded[13..]);
        unity
    }

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
    fn gzip_decompression_rejects_expansion_ratio_with_structured_budget_error() {
        let original = vec![0_u8; 8 * 1024];
        let compressed = gzip_compress(&original);
        let mut budget = budget_with_decompression_limits(16 * 1024, 2);

        let error = decompress_gzip_with_budget(&compressed, &mut budget).unwrap_err();
        let observed = match error {
            BinaryError::Budget(BudgetError::ExpansionRatioExceeded {
                compressed_bytes,
                decompressed_bytes,
                max_ratio,
            }) => (compressed_bytes, decompressed_bytes, max_ratio),
            other => panic!("expected expansion-ratio error, got {other}"),
        };
        assert!(observed.0 > 0 && observed.0 <= compressed.len() as u64);
        assert_eq!(observed.1, 8_192);
        assert_eq!(observed.2, 2);
        assert_eq!(budget.usage().compressed_bytes, observed.0);
        assert_eq!(budget.usage().decompressed_bytes, 0);

        let mut padded = compressed.clone();
        padded.resize(compressed.len() + original.len(), 0);
        let mut padded_budget = budget_with_decompression_limits(16 * 1024, 2);
        let padded_error = decompress_gzip_with_budget(&padded, &mut padded_budget).unwrap_err();
        let padded_observed = match padded_error {
            BinaryError::Budget(BudgetError::ExpansionRatioExceeded {
                compressed_bytes,
                decompressed_bytes,
                max_ratio,
            }) => (compressed_bytes, decompressed_bytes, max_ratio),
            other => panic!("expected expansion-ratio error, got {other}"),
        };

        assert_eq!(padded_observed, observed);
        assert_eq!(padded_budget.usage(), budget.usage());
    }

    #[test]
    fn gzip_decompression_rejects_bytes_after_a_valid_member() {
        let original = b"one gzip member";
        let compressed = gzip_compress(original);
        let mut padded = compressed.clone();
        padded.push(0);
        let mut budget = AssetLoadBudget::default();

        let error = decompress_gzip_with_budget(&padded, &mut budget).unwrap_err();

        assert!(matches!(
            error,
            BinaryError::DecompressionFailed(message)
                if message.contains("1 trailing bytes")
                    && message.contains("single member")
        ));
        assert_eq!(budget.usage().compressed_bytes, compressed.len() as u64);
        assert_eq!(budget.usage().decompressed_bytes, original.len() as u64);
    }

    #[test]
    fn brotli_decompression_rejects_expansion_ratio_with_structured_budget_error() {
        let original = vec![0_u8; 8 * 1024];
        let compressed = brotli_compress(&original);
        let mut budget = budget_with_decompression_limits(16 * 1024, 2);

        let error = decompress_brotli_with_budget(&compressed, &mut budget).unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::ExpansionRatioExceeded {
                compressed_bytes,
                decompressed_bytes: 8_192,
                max_ratio: 2,
            }) if compressed_bytes == compressed.len() as u64
        ));
        assert_eq!(budget.usage().compressed_bytes, compressed.len() as u64);
        assert_eq!(budget.usage().decompressed_bytes, 0);
    }

    #[test]
    fn brotli_declared_size_stops_overproduction_during_decode() {
        let original = vec![0x5a; 4096];
        let compressed = brotli_compress(&original);
        let mut budget = AssetLoadBudget::default();

        let error = decompress_with_budget(
            &compressed,
            CompressionType::Brotli,
            original.len() - 1,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::ResourceLimitExceeded(message)
                if message.contains("exceeds declared limit")
        ));
        assert_eq!(budget.usage().compressed_bytes, compressed.len() as u64);
        assert_eq!(budget.usage().decompressed_bytes, 0);
    }

    #[test]
    fn brotli_decompression_rejects_underproduction() {
        let original = vec![0x5a; 4096];
        let compressed = brotli_compress(&original);
        let declared_size = original.len() + 1;
        let mut budget = AssetLoadBudget::default();

        let error = decompress_with_budget(
            &compressed,
            CompressionType::Brotli,
            declared_size,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::DecompressionFailed(message)
                if message.contains("Brotli decompression size mismatch")
                    && message.contains(&format!("expected {declared_size}"))
                    && message.contains(&format!("got {}", original.len()))
        ));
        assert_eq!(budget.usage().compressed_bytes, compressed.len() as u64);
        assert_eq!(budget.usage().decompressed_bytes, original.len() as u64);
    }

    #[test]
    fn lz4_decompression_rejects_declared_output_before_decode() {
        let original = vec![0x5a_u8; 4 * 1024];
        let compressed = lz4_flex::block::compress(&original);
        let mut budget = budget_with_decompression_limits(128, 1_024);

        let error = decompress_with_budget(
            &compressed,
            CompressionType::Lz4,
            original.len(),
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "decompressed_bytes",
                limit: 128,
                requested: 4_096,
            })
        ));
        assert_eq!(budget.usage().compressed_bytes, 0);
        assert_eq!(budget.usage().decompressed_bytes, 0);
    }

    #[test]
    fn lzma_preserves_compressed_budget_errors() {
        let encoded = [0_u8; 13];
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_compressed_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = decompress_with_budget(&encoded, CompressionType::Lzma, 64, &mut budget)
            .expect_err("LZMA must preserve a hard compressed-input budget error");
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "compressed_bytes",
                ..
            })
        ));
        assert_eq!(budget.usage().compressed_bytes, 0);
    }

    #[test]
    fn lzma_size_stream_obeys_exact_cumulative_budgets() {
        let original = vec![0xA5; 4096];
        let compressed = lzma_compress(&original);
        let compressed_len = u64::try_from(compressed.len()).unwrap();
        let decompressed_len = u64::try_from(original.len()).unwrap();
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_compressed_bytes: compressed_len,
            max_decompressed_bytes: decompressed_len,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let decoded =
            decompress_lzma_size_stream_with_budget(&compressed, original.len(), &mut exact)
                .unwrap();
        assert_eq!(decoded, original);
        assert_eq!(exact.usage().compressed_bytes, compressed_len);
        assert_eq!(exact.usage().decompressed_bytes, decompressed_len);
        assert_eq!(exact.usage().bytes, 18_976);

        let mut compressed_short = AssetLoadBudget::new(AssetLoadLimits {
            max_compressed_bytes: compressed_len - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            decompress_lzma_size_stream_with_budget(
                &compressed,
                original.len(),
                &mut compressed_short,
            ),
            Err(BinaryError::Budget(BudgetError::Exceeded {
                resource: "compressed_bytes",
                ..
            }))
        ));
        assert_eq!(compressed_short.usage().compressed_bytes, 0);
        assert_eq!(compressed_short.usage().decompressed_bytes, 0);

        let mut decompressed_short = AssetLoadBudget::new(AssetLoadLimits {
            max_decompressed_bytes: decompressed_len - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            decompress_lzma_size_stream_with_budget(
                &compressed,
                original.len(),
                &mut decompressed_short,
            ),
            Err(BinaryError::Budget(BudgetError::Exceeded {
                resource: "decompressed_bytes",
                ..
            }))
        ));
        assert_eq!(decompressed_short.usage().compressed_bytes, 0);
        assert_eq!(decompressed_short.usage().decompressed_bytes, 0);
    }

    #[test]
    fn lzma_size_stream_rejects_an_embedded_size_mismatch_before_decode() {
        let original = vec![0x5a; 1024];
        let compressed = lzma_compress(&original);
        let mut budget = AssetLoadBudget::default();

        let error =
            decompress_lzma_size_stream_with_budget(&compressed, original.len() - 1, &mut budget)
                .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::InvalidData(message)
                if message.contains("header declares 1024 bytes")
                    && message.contains("container declares 1023")
        ));
        assert_eq!(budget.usage().compressed_bytes, 0);
        assert_eq!(budget.usage().decompressed_bytes, 0);
    }

    #[test]
    fn lzma_decompression_rejects_underproduction() {
        let original = vec![0x3c; 4096];
        let compressed = lzma_compress_unknown_size(&original);
        let declared_size = original.len() + 1;
        let mut budget = AssetLoadBudget::default();

        let error =
            decompress_lzma_size_stream_with_budget(&compressed, declared_size, &mut budget)
                .unwrap_err();

        assert!(matches!(
            error,
            BinaryError::DecompressionFailed(message)
                if message.contains("LZMA decompression size mismatch")
                    && message.contains(&format!("expected {declared_size}"))
                    && message.contains(&format!("got {}", original.len()))
        ));
        assert_eq!(budget.usage().compressed_bytes, compressed.len() as u64);
        assert_eq!(budget.usage().decompressed_bytes, original.len() as u64);
    }

    #[test]
    fn unity_lzma_properties_stream_uses_the_container_size() {
        let original = vec![0x3c; 4096];
        let compressed = unity_lzma_compress(&original);
        let mut budget = AssetLoadBudget::default();

        let decoded = decompress_with_budget(
            &compressed,
            CompressionType::Lzma,
            original.len(),
            &mut budget,
        )
        .unwrap();

        assert_eq!(decoded, original);
        assert_eq!(budget.usage().compressed_bytes, compressed.len() as u64);
        assert_eq!(budget.usage().decompressed_bytes, original.len() as u64);
    }

    #[test]
    fn lzma_decoder_scratch_is_preflighted_before_an_adversarial_dictionary() {
        const EXPECTED_SCRATCH_BYTES: u64 = 18_976;

        let original = vec![0x3c; 4096];
        let mut compressed = unity_lzma_compress(&original);
        compressed[1..5].copy_from_slice(&u32::MAX.to_le_bytes());

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: EXPECTED_SCRATCH_BYTES - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = decompress_with_budget(
            &compressed,
            CompressionType::Lzma,
            original.len(),
            &mut short,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == EXPECTED_SCRATCH_BYTES - 1
                && requested == EXPECTED_SCRATCH_BYTES
        ));
        assert_eq!(short.usage(), Default::default());

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: EXPECTED_SCRATCH_BYTES,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let decoded = decompress_with_budget(
            &compressed,
            CompressionType::Lzma,
            original.len(),
            &mut exact,
        )
        .unwrap();
        assert_eq!(decoded, original);
        assert_eq!(exact.usage().bytes, EXPECTED_SCRATCH_BYTES);
    }

    #[test]
    fn lzma_memory_plan_accounts_for_dictionary_capacity_growth() {
        let plan = lzma_memory_plan(0x5d, u32::MAX, 5_000).unwrap();

        assert_eq!(plan.dictionary_limit, 5_000);
        assert_eq!(plan.scratch_bytes, 23_072);
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
