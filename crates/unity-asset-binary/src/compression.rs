//! Compression support for Unity binary files

use crate::error::{BinaryError, Result};
use crate::random_access::{BorrowedBytes, ByteSource, FallibleBufReader};
use brotli::{Allocator, SliceWrapper, SliceWrapperMut};
use flate2::bufread::GzDecoder;
use std::cell::Cell;
use std::io::{BufRead, Read, Write};
use unity_asset_core::{AssetLoadBudget, BudgetError, DecompressionBudget};

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

struct PrefixBudgetedOutput<'a> {
    budget: DecompressionBudget<'a>,
    prefix: Vec<u8>,
    prefix_limit: usize,
    decoded_len: u64,
    expected_len: u64,
    write_failure: Option<BinaryError>,
}

impl<'a> PrefixBudgetedOutput<'a> {
    fn new(
        load_budget: &'a mut AssetLoadBudget,
        compressed_size: u64,
        expected_len: u64,
        prefix_limit: usize,
        prefix: Vec<u8>,
    ) -> Result<Self> {
        let mut budget = load_budget.begin_decompression();
        budget.consume(compressed_size, 0)?;
        Ok(Self {
            budget,
            prefix,
            prefix_limit,
            decoded_len: 0,
            expected_len,
            write_failure: None,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        let chunk_len = usize_to_u64(bytes.len(), "LZMA decoded chunk size")?;
        let decoded_len = self
            .decoded_len
            .checked_add(chunk_len)
            .ok_or_else(|| BinaryError::invalid_data("LZMA decoded length overflow"))?;
        if decoded_len > self.expected_len {
            return Err(BinaryError::decompression_failed(format!(
                "LZMA decompression exceeded declared size {}",
                self.expected_len
            )));
        }
        self.budget.consume(0, chunk_len)?;

        let remaining_prefix = self.prefix_limit.saturating_sub(self.prefix.len());
        let retained = remaining_prefix.min(bytes.len());
        self.prefix.extend_from_slice(&bytes[..retained]);
        self.decoded_len = decoded_len;
        Ok(())
    }

    fn finish(self) -> Result<PrefixOutput> {
        if let Some(error) = self.write_failure {
            return Err(error);
        }
        Ok(PrefixOutput {
            prefix: self.prefix,
            decoded_len: self.decoded_len,
        })
    }
}

impl Write for PrefixBudgetedOutput<'_> {
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

struct PrefixOutput {
    prefix: Vec<u8>,
    decoded_len: u64,
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
    let header = lzma_alone_header(properties, dictionary_size);
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

const LZMA_INSPECTION_BUFFER_SIZE: usize = 64 * 1024;

/// Bounded result for callers that only need the metadata prefix of a legacy LZMA stream.
pub(crate) struct LzmaStreamInspection {
    pub(crate) prefix: Vec<u8>,
    pub(crate) decoded_len: u64,
    pub(crate) max_temporary_bytes: u64,
}

/// Streams a legacy LZMA-alone payload while retaining only its declared metadata prefix.
pub(crate) fn inspect_lzma_size_stream_with_budget(
    mut input: impl Read,
    compressed_size: u64,
    uncompressed_size: u64,
    retained_prefix_size: usize,
    budget: &mut AssetLoadBudget,
) -> Result<LzmaStreamInspection> {
    let compressed_size_usize = usize::try_from(compressed_size)
        .map_err(|_| BinaryError::invalid_data("LZMA input size does not fit usize"))?;
    let uncompressed_size_usize = usize::try_from(uncompressed_size)
        .map_err(|_| BinaryError::invalid_data("LZMA output size does not fit usize"))?;
    if compressed_size_usize < 13 {
        return Err(BinaryError::invalid_data(
            "Unity LZMA size stream is shorter than its 13-byte header",
        ));
    }
    if retained_prefix_size > uncompressed_size_usize {
        return Err(BinaryError::invalid_data(format!(
            "LZMA metadata prefix {retained_prefix_size} exceeds declared output {uncompressed_size_usize}"
        )));
    }
    budget.check_decompression(compressed_size, uncompressed_size)?;

    let mut header = [0_u8; 13];
    input.read_exact(&mut header).map_err(|error| {
        BinaryError::decompression_failed(format!("Failed to read the Unity LZMA header: {error}"))
    })?;
    let properties = header[0];
    let dictionary_size = u32::from_le_bytes(
        header[1..5]
            .try_into()
            .map_err(|_| BinaryError::invalid_data("Invalid LZMA dictionary size width"))?,
    );
    let declared_size = u64::from_le_bytes(
        header[5..13]
            .try_into()
            .map_err(|_| BinaryError::invalid_data("Invalid LZMA declared size width"))?,
    );
    if declared_size != uncompressed_size && declared_size != u64::MAX {
        return Err(BinaryError::invalid_data(format!(
            "LZMA header declares {declared_size} bytes but the container declares {uncompressed_size}"
        )));
    }

    let memory = lzma_memory_plan(properties, dictionary_size, uncompressed_size_usize)?;
    let input_buffer_bytes = usize_to_u64(
        LZMA_INSPECTION_BUFFER_SIZE,
        "LZMA inspection input buffer size",
    )?;
    let retained_prefix_bytes = usize_to_u64(retained_prefix_size, "LZMA metadata prefix size")?;
    let max_temporary_bytes = memory
        .scratch_bytes
        .checked_add(input_buffer_bytes)
        .and_then(|bytes| bytes.checked_add(retained_prefix_bytes))
        .ok_or_else(|| BinaryError::memory_error("LZMA inspection working set overflow"))?;
    budget.consume_bytes(max_temporary_bytes)?;

    let mut prefix = Vec::new();
    prefix
        .try_reserve_exact(retained_prefix_size)
        .map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {retained_prefix_size} legacy metadata bytes: {error}"
            ))
        })?;
    let reconstructed = std::io::Cursor::new(header).chain(input);
    let mut encoded =
        FallibleBufReader::try_with_capacity(LZMA_INSPECTION_BUFFER_SIZE, reconstructed)?;
    let options = lzma_rs::decompress::Options {
        memlimit: Some(memory.dictionary_limit),
        ..Default::default()
    };
    let mut output = PrefixBudgetedOutput::new(
        budget,
        compressed_size,
        uncompressed_size,
        retained_prefix_size,
        prefix,
    )?;
    let decoded = lzma_rs::lzma_decompress_with_options(&mut encoded, &mut output, &options);
    let output = output.finish()?;
    decoded.map_err(|error| {
        let message = error.to_string();
        if message.contains("Found end-of-stream marker but more bytes are available") {
            BinaryError::decompression_failed("LZMA stream contains trailing bytes")
        } else {
            BinaryError::decompression_failed(format!("LZMA decompression failed: {message}"))
        }
    })?;
    if output.decoded_len != uncompressed_size {
        return Err(BinaryError::decompression_failed(format!(
            "LZMA decompression size mismatch: expected {uncompressed_size}, got {}",
            output.decoded_len
        )));
    }
    if output.prefix.len() != retained_prefix_size {
        return Err(BinaryError::decompression_failed(format!(
            "LZMA metadata prefix size mismatch: expected {retained_prefix_size}, got {}",
            output.prefix.len()
        )));
    }
    let trailing = encoded.fill_buf().map_err(|error| {
        BinaryError::decompression_failed(format!(
            "Failed to inspect the end of the LZMA stream: {error}"
        ))
    })?;
    if !trailing.is_empty() {
        return Err(BinaryError::decompression_failed(format!(
            "LZMA stream contains {} trailing bytes",
            trailing.len()
        )));
    }

    Ok(LzmaStreamInspection {
        prefix: output.prefix,
        decoded_len: output.decoded_len,
        max_temporary_bytes,
    })
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
const BROTLI_INPUT_BUFFER_SIZE: usize = 64 * 1024;
const BROTLI_MAX_WINDOW_BITS: u32 = 30;

// These sizes mirror the conservative no-std preallocation strategy in
// brotli-decompressor 5.0.3. The input buffer is additional reader-owned scratch.
const BROTLI_PREALLOC_U8_FIXED_BYTES: u64 = 64 * 1024 + (256 + 704) * 256;
const BROTLI_PREALLOC_U32_ELEMENTS: u64 = 12 * 1024 * 6;
const BROTLI_PREALLOC_HUFFMAN_ELEMENTS: u64 = 128 * (704 + 256) + 6 * 26 * 1_080;

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
        CompressionType::Brotli => brotli_planning_scratch_bytes(data),
        CompressionType::None | CompressionType::Lz4 | CompressionType::Lz4Hc => Ok(0),
    }
}

fn brotli_planning_scratch_bytes(data: &[u8]) -> Result<u64> {
    // Brotli's exact allocation sequence depends on encoded metablock tables. Planning uses the
    // upstream reusable-pool peak bound; the custom allocator below charges every runtime
    // allocation monotonically. Invalid or truncated headers take the maximum 30-bit window.
    let window_bytes = 1_u64
        .checked_shl(brotli_window_bits(data))
        .ok_or_else(|| BinaryError::memory_error("Brotli window size overflow"))?;
    let window_pool_bytes = window_bytes
        .checked_add(window_bytes / 4)
        .ok_or_else(|| BinaryError::memory_error("Brotli window scratch size overflow"))?;
    let u32_bytes = BROTLI_PREALLOC_U32_ELEMENTS
        .checked_mul(usize_to_u64(
            std::mem::size_of::<u32>(),
            "u32 element size",
        )?)
        .ok_or_else(|| BinaryError::memory_error("Brotli u32 scratch size overflow"))?;
    let huffman_bytes = BROTLI_PREALLOC_HUFFMAN_ELEMENTS
        .checked_mul(usize_to_u64(
            std::mem::size_of::<brotli::HuffmanCode>(),
            "Brotli Huffman element size",
        )?)
        .ok_or_else(|| BinaryError::memory_error("Brotli Huffman scratch size overflow"))?;

    usize_to_u64(BROTLI_INPUT_BUFFER_SIZE, "Brotli input buffer size")?
        .checked_add(BROTLI_PREALLOC_U8_FIXED_BYTES)
        .and_then(|bytes| bytes.checked_add(window_pool_bytes))
        .and_then(|bytes| bytes.checked_add(u32_bytes))
        .and_then(|bytes| bytes.checked_add(huffman_bytes))
        .ok_or_else(|| BinaryError::memory_error("Brotli planning scratch size overflow"))
}

fn brotli_window_bits(data: &[u8]) -> u32 {
    let Some(&first) = data.first() else {
        return BROTLI_MAX_WINDOW_BITS;
    };
    if first & 1 == 0 {
        return 16;
    }

    let standard = match first & 0x0f {
        0x03 => Some(18),
        0x05 => Some(19),
        0x07 => Some(20),
        0x09 => Some(21),
        0x0b => Some(22),
        0x0d => Some(23),
        0x0f => Some(24),
        _ => match first & 0x7f {
            0x71 => Some(15),
            0x61 => Some(14),
            0x51 => Some(13),
            0x41 => Some(12),
            0x31 => Some(11),
            0x21 => Some(10),
            0x01 => Some(17),
            _ => None,
        },
    };
    if let Some(bits) = standard {
        return bits;
    }

    data.get(1)
        .copied()
        .filter(|_| first & 0x80 == 0)
        .map(|second| u32::from(second & 0x3f))
        .filter(|bits| (10..=BROTLI_MAX_WINDOW_BITS).contains(bits))
        .unwrap_or(BROTLI_MAX_WINDOW_BITS)
}

fn lzma_alone_header(properties: u8, dictionary_size: u32) -> [u8; 13] {
    let mut header = [0_u8; 13];
    header[0] = properties;
    header[1..5].copy_from_slice(&dictionary_size.to_le_bytes());
    // Unity's five-byte layout stores the output size in the container. Requiring
    // the LZMA end marker makes the codec consume and validate its entire range.
    header[5..13].copy_from_slice(&u64::MAX.to_le_bytes());
    header
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
        let message = error.to_string();
        if message.contains("Found end-of-stream marker but more bytes are available") {
            BinaryError::decompression_failed("LZMA stream contains trailing bytes")
        } else {
            BinaryError::decompression_failed(format!("LZMA decompression failed: {message}"))
        }
    })?;
    let output = validate_declared_size("LZMA", output, expected_size)?;
    let trailing = encoded.fill_buf().map_err(|error| {
        BinaryError::decompression_failed(format!(
            "Failed to inspect the end of the LZMA stream: {error}"
        ))
    })?;
    if !trailing.is_empty() {
        return Err(BinaryError::decompression_failed(format!(
            "LZMA stream contains {} trailing bytes",
            trailing.len()
        )));
    }
    Ok(output)
}

/// Decompress Brotli compressed data (used in WebGL builds)
pub fn decompress_brotli(data: &[u8]) -> Result<Vec<u8>> {
    let mut budget = AssetLoadBudget::default();
    decompress_brotli_with_budget(data, &mut budget)
}

/// Decompress Brotli data with bounded streaming output.
pub fn decompress_brotli_with_budget(data: &[u8], budget: &mut AssetLoadBudget) -> Result<Vec<u8>> {
    budget.check_compressed_bytes(usize_to_u64(data.len(), "Brotli input size")?)?;
    decompress_brotli_stream_with_budget(data, None, budget)
}

fn decompress_brotli_exact_with_budget(
    data: &[u8],
    expected_size: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    let output = decompress_brotli_stream_with_budget(data, Some(expected_size), budget)?;
    validate_declared_size("Brotli", output, expected_size)
}

fn decompress_brotli_stream_with_budget(
    data: &[u8],
    maximum_output: Option<usize>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>> {
    let scratch = BrotliScratchReservation::new(budget);
    let source = BorrowedBytes::new(data);
    let decoder = SegmentedBrotliDecoder::new(&source, &scratch);

    let decode_result = if scratch.has_failure() {
        None
    } else {
        Some(decompress_reader_with_budget(
            decoder,
            "Brotli",
            data.len(),
            maximum_output,
            budget,
        ))
    };
    let scratch_result = scratch.commit(budget);

    match (scratch_result, decode_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Some(result)) => result,
        (Ok(()), None) => Err(BinaryError::memory_error(
            "Brotli scratch allocation failed without an allocator error",
        )),
    }
}

struct BrotliScratchMemory<T>(Vec<T>);

impl<T> Default for BrotliScratchMemory<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> SliceWrapper<T> for BrotliScratchMemory<T> {
    fn slice(&self) -> &[T] {
        &self.0
    }
}

impl<T> SliceWrapperMut<T> for BrotliScratchMemory<T> {
    fn slice_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

/// Deferred byte accounting shared by a parser and its codec allocator.
///
/// Reservations are checked against one combined total before allocation and
/// committed to the caller-owned budget in one operation.
pub(crate) struct DeferredByteReservation {
    initial_bytes: u64,
    max_bytes: u64,
    metadata_bytes: Cell<u64>,
    scratch_bytes: Cell<u64>,
}

impl DeferredByteReservation {
    pub(crate) fn new(budget: &AssetLoadBudget) -> Self {
        Self {
            initial_bytes: budget.usage().bytes,
            max_bytes: budget.limits().max_bytes,
            metadata_bytes: Cell::new(0),
            scratch_bytes: Cell::new(0),
        }
    }

    pub(crate) fn reserve_metadata(&self, amount: u64) -> Result<()> {
        self.check_additional(amount)?;
        self.metadata_bytes
            .set(
                self.metadata_bytes
                    .get()
                    .checked_add(amount)
                    .ok_or(BinaryError::Budget(BudgetError::ArithmeticOverflow {
                        resource: "bytes",
                    }))?,
            );
        Ok(())
    }

    pub(crate) fn reserve_scratch(&self, amount: u64) -> Result<()> {
        self.check_scratch(amount)?;
        self.record_scratch(amount)?;
        Ok(())
    }

    fn check_scratch(&self, amount: u64) -> std::result::Result<(), BudgetError> {
        self.check_additional_budget(amount)
    }

    fn record_scratch(&self, amount: u64) -> std::result::Result<(), BudgetError> {
        let scratch = self
            .scratch_bytes
            .get()
            .checked_add(amount)
            .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        self.scratch_bytes.set(scratch);
        Ok(())
    }

    fn check_additional(&self, amount: u64) -> Result<()> {
        self.check_additional_budget(amount).map_err(Into::into)
    }

    fn check_additional_budget(&self, amount: u64) -> std::result::Result<(), BudgetError> {
        let deferred = self
            .metadata_bytes
            .get()
            .checked_add(self.scratch_bytes.get())
            .and_then(|bytes| bytes.checked_add(amount))
            .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        let requested = self
            .initial_bytes
            .checked_add(deferred)
            .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        if requested > self.max_bytes {
            return Err(BudgetError::Exceeded {
                resource: "bytes",
                limit: self.max_bytes,
                requested,
            });
        }
        Ok(())
    }

    pub(crate) fn metadata_bytes(&self) -> u64 {
        self.metadata_bytes.get()
    }

    pub(crate) fn commit(&self, budget: &mut AssetLoadBudget) -> Result<()> {
        let total = self
            .metadata_bytes
            .get()
            .checked_add(self.scratch_bytes.get())
            .ok_or(BinaryError::Budget(BudgetError::ArithmeticOverflow {
                resource: "bytes",
            }))?;
        budget.consume_bytes(total).map_err(Into::into)
    }
}

#[derive(Clone, Copy)]
struct BrotliScratchAllocator<'reservation, 'ledger> {
    reservation: &'reservation BrotliScratchReservation<'ledger>,
}

impl<'reservation, 'ledger> BrotliScratchAllocator<'reservation, 'ledger> {
    fn new(reservation: &'reservation BrotliScratchReservation<'ledger>) -> Self {
        Self { reservation }
    }
}

impl<T: Default> Allocator<T> for BrotliScratchAllocator<'_, '_> {
    type AllocatedMemory = BrotliScratchMemory<T>;

    fn alloc_cell(&mut self, len: usize) -> Self::AllocatedMemory {
        self.reservation
            .allocate(len)
            .map(BrotliScratchMemory)
            .unwrap_or_default()
    }

    fn free_cell(&mut self, _memory: Self::AllocatedMemory) {}
}

pub(crate) struct BrotliScratchReservation<'ledger> {
    initial_bytes: u64,
    max_bytes: u64,
    successful_bytes: Cell<u64>,
    deferred: Option<&'ledger DeferredByteReservation>,
    failed: Cell<bool>,
    failure: Cell<Option<BrotliScratchFailure>>,
}

impl BrotliScratchReservation<'static> {
    pub(crate) fn new(budget: &AssetLoadBudget) -> Self {
        Self {
            initial_bytes: budget.usage().bytes,
            max_bytes: budget.limits().max_bytes,
            successful_bytes: Cell::new(0),
            deferred: None,
            failed: Cell::new(false),
            failure: Cell::new(None),
        }
    }
}

impl<'ledger> BrotliScratchReservation<'ledger> {
    pub(crate) fn with_deferred(deferred: &'ledger DeferredByteReservation) -> Self {
        Self {
            initial_bytes: 0,
            max_bytes: 0,
            successful_bytes: Cell::new(0),
            deferred: Some(deferred),
            failed: Cell::new(false),
            failure: Cell::new(None),
        }
    }

    fn allocate<T: Default>(&self, len: usize) -> Option<Vec<T>> {
        if self.has_failure() {
            return None;
        }

        let allocation_bytes = len
            .checked_mul(std::mem::size_of::<T>())
            .and_then(|bytes| u64::try_from(bytes).ok());
        let Some(allocation_bytes) = allocation_bytes else {
            self.record_failure(BrotliScratchFailure::Budget(
                BudgetError::ArithmeticOverflow { resource: "bytes" },
            ));
            return None;
        };
        if let Some(deferred) = self.deferred {
            if let Err(error) = deferred.check_scratch(allocation_bytes) {
                self.record_failure(BrotliScratchFailure::Budget(error));
                return None;
            }
        } else {
            let requested = self
                .initial_bytes
                .checked_add(self.successful_bytes.get())
                .and_then(|bytes| bytes.checked_add(allocation_bytes));
            let Some(requested) = requested else {
                self.record_failure(BrotliScratchFailure::Budget(
                    BudgetError::ArithmeticOverflow { resource: "bytes" },
                ));
                return None;
            };
            if requested > self.max_bytes {
                self.record_failure(BrotliScratchFailure::Budget(BudgetError::Exceeded {
                    resource: "bytes",
                    limit: self.max_bytes,
                    requested,
                }));
                return None;
            }
        }

        let mut memory = Vec::new();
        if let Err(error) = memory.try_reserve_exact(len) {
            self.record_failure(BrotliScratchFailure::Allocation {
                requested: allocation_bytes,
                error,
            });
            return None;
        }
        memory.resize_with(len, T::default);
        if let Some(deferred) = self.deferred {
            if let Err(error) = deferred.record_scratch(allocation_bytes) {
                self.record_failure(BrotliScratchFailure::Budget(error));
                return None;
            }
        } else {
            self.successful_bytes
                .set(self.successful_bytes.get() + allocation_bytes);
        }
        Some(memory)
    }

    fn has_failure(&self) -> bool {
        self.failed.get()
    }

    fn record_failure(&self, failure: BrotliScratchFailure) {
        if !self.failed.replace(true) {
            self.failure.set(Some(failure));
        }
    }

    pub(crate) fn commit(&self, budget: &mut AssetLoadBudget) -> Result<()> {
        if self.deferred.is_none() {
            budget.consume_bytes(self.successful_bytes.get())?;
        }
        self.finish()
    }

    pub(crate) fn finish(&self) -> Result<()> {
        match self.failure.take() {
            Some(BrotliScratchFailure::Budget(error)) => Err(BinaryError::Budget(error)),
            Some(BrotliScratchFailure::Allocation { requested, error }) => {
                Err(BinaryError::memory_error(format!(
                    "Failed to reserve {requested} bytes for Brotli decoder scratch: {error}"
                )))
            }
            None => Ok(()),
        }
    }
}

/// Streaming Brotli decoder that retains exact input-consumption state.
pub(crate) struct SegmentedBrotliDecoder<'source, 'scratch, 'ledger> {
    source: &'source dyn ByteSource,
    source_offset: u64,
    input: BrotliScratchMemory<u8>,
    available_in: usize,
    input_offset: usize,
    total_out: usize,
    state: brotli::BrotliState<
        BrotliScratchAllocator<'scratch, 'ledger>,
        BrotliScratchAllocator<'scratch, 'ledger>,
        BrotliScratchAllocator<'scratch, 'ledger>,
    >,
    done: bool,
    pending_error: Option<String>,
}

impl<'source, 'scratch, 'ledger> SegmentedBrotliDecoder<'source, 'scratch, 'ledger> {
    pub(crate) fn new(
        source: &'source dyn ByteSource,
        scratch: &'scratch BrotliScratchReservation<'ledger>,
    ) -> Self {
        let mut input_allocator = BrotliScratchAllocator::new(scratch);
        let input = input_allocator.alloc_cell(BROTLI_INPUT_BUFFER_SIZE);
        Self {
            source,
            source_offset: 0,
            input,
            available_in: 0,
            input_offset: 0,
            total_out: 0,
            state: brotli::BrotliState::new(
                input_allocator,
                BrotliScratchAllocator::new(scratch),
                BrotliScratchAllocator::new(scratch),
            ),
            done: false,
            pending_error: None,
        }
    }

    fn refill(&mut self) -> std::io::Result<bool> {
        if self.source_offset == self.source.len() {
            return Ok(false);
        }
        if self.input.slice().is_empty() {
            return Err(std::io::Error::other(
                "Brotli input buffer allocation failed",
            ));
        }
        let remaining = self
            .source
            .len()
            .checked_sub(self.source_offset)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Brotli source position moved past the source",
                )
            })?;
        let read_len = remaining.min(u64::try_from(self.input.slice().len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Brotli input buffer size does not fit u64",
            )
        })?);
        let read_len = usize::try_from(read_len).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Brotli source chunk size does not fit usize",
            )
        })?;
        self.source
            .read_exact_at(self.source_offset, &mut self.input.slice_mut()[..read_len])
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.source_offset = self
            .source_offset
            .checked_add(u64::try_from(read_len).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Brotli source chunk size does not fit u64",
                )
            })?)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Brotli source position overflow",
                )
            })?;
        self.available_in = read_len;
        self.input_offset = 0;
        Ok(true)
    }

    fn fail_or_defer(&mut self, produced: usize, message: String) -> std::io::Result<usize> {
        if produced == 0 {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                message,
            ))
        } else {
            self.pending_error = Some(message);
            Ok(produced)
        }
    }
}

impl Read for SegmentedBrotliDecoder<'_, '_, '_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if let Some(message) = self.pending_error.take() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                message,
            ));
        }
        if self.done {
            return Ok(0);
        }

        loop {
            if self.available_in == 0 {
                self.refill()?;
            }
            let mut available_out = output.len();
            let mut output_offset = 0_usize;
            let result = brotli::BrotliDecompressStream(
                &mut self.available_in,
                &mut self.input_offset,
                self.input.slice(),
                &mut available_out,
                &mut output_offset,
                output,
                &mut self.total_out,
                &mut self.state,
            );
            let produced = output_offset;
            match result {
                brotli::BrotliResult::ResultSuccess => {
                    self.done = true;
                    let unloaded = self
                        .source
                        .len()
                        .checked_sub(self.source_offset)
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "Brotli source position moved past the source",
                            )
                        })?;
                    let buffered = u64::try_from(self.available_in).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Brotli buffered input size does not fit u64",
                        )
                    })?;
                    let trailing = buffered.checked_add(unloaded).ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Brotli trailing input size overflow",
                        )
                    })?;
                    if trailing != 0 {
                        return self.fail_or_defer(
                            produced,
                            format!("Brotli stream contains {trailing} trailing bytes"),
                        );
                    }
                    return Ok(produced);
                }
                brotli::BrotliResult::NeedsMoreInput => {
                    if produced != 0 {
                        return Ok(produced);
                    }
                    if self.available_in != 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Brotli decoder requested input before consuming its current chunk",
                        ));
                    }
                    if self.source_offset == self.source.len() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "Brotli stream ended before its terminator",
                        ));
                    }
                }
                brotli::BrotliResult::NeedsMoreOutput => {
                    if produced == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Brotli decoder requested output without producing bytes",
                        ));
                    }
                    return Ok(produced);
                }
                brotli::BrotliResult::ResultFailure => {
                    return self.fail_or_defer(produced, "Brotli decompression failed".to_string());
                }
            }
        }
    }
}

enum BrotliScratchFailure {
    Budget(BudgetError),
    Allocation {
        requested: u64,
        error: std::collections::TryReserveError,
    },
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
    fn brotli_input_buffer_is_preflighted_against_the_byte_budget() {
        const INPUT_BUFFER_BYTES: u64 = 64 * 1024;
        const PREEXISTING_BYTES: u64 = 17;
        const EXPECTED_BYTES: u64 = PREEXISTING_BYTES + INPUT_BUFFER_BYTES;

        let compressed = brotli_compress(b"budgeted Brotli input buffer");
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: EXPECTED_BYTES - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        budget.consume_bytes(PREEXISTING_BYTES).unwrap();

        let error = decompress_brotli_with_budget(&compressed, &mut budget).unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == EXPECTED_BYTES - 1 && requested == EXPECTED_BYTES
        ));
        assert_eq!(budget.usage().bytes, PREEXISTING_BYTES);
        assert_eq!(budget.usage().compressed_bytes, 0);
        assert_eq!(budget.usage().decompressed_bytes, 0);
    }

    #[test]
    fn brotli_dynamic_allocations_are_preflighted_against_the_byte_budget() {
        const INPUT_BUFFER_BYTES: u64 = 64 * 1024;
        // Locks the full allocation sequence for this brotli-decompressor 5.0.3 fixture.
        const ALLOCATION_BOUNDARIES: &[u64] = &[
            69_856, 82_816, 95_776, 104_034, 104_035, 104_099, 104_103, 104_107, 108_427, 108_431,
            112_751, 112_755, 117_075,
        ];
        const EXPECTED_SCRATCH_BYTES: u64 = 117_075;

        let original = vec![0x5a; 4096];
        let compressed = brotli_compress(&original);
        for &expected in ALLOCATION_BOUNDARIES {
            let mut budget = AssetLoadBudget::new(AssetLoadLimits {
                max_bytes: expected - 1,
                ..AssetLoadLimits::default()
            })
            .unwrap();

            let error = decompress_brotli_with_budget(&compressed, &mut budget).unwrap_err();

            assert!(matches!(
                error,
                BinaryError::Budget(BudgetError::Exceeded {
                    resource: "bytes",
                    limit,
                    requested,
                }) if limit == expected - 1 && requested == expected
            ));
            assert!(budget.usage().bytes >= INPUT_BUFFER_BYTES);
            assert!(budget.usage().bytes < expected);
            assert_eq!(budget.usage().decompressed_bytes, 0);
        }

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: EXPECTED_SCRATCH_BYTES,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert_eq!(
            decompress_brotli_with_budget(&compressed, &mut exact).unwrap(),
            original
        );
        assert_eq!(exact.usage().bytes, EXPECTED_SCRATCH_BYTES);
    }

    #[test]
    fn brotli_planning_scratch_uses_the_upstream_preallocation_bound() {
        const STANDARD_WINDOW_SCRATCH_BYTES: u64 = 7_080_064;
        const LARGE_WINDOW_SCRATCH_BYTES: u64 = 1_344_014_464;

        assert_eq!(std::mem::size_of::<brotli::HuffmanCode>(), 4);
        assert_eq!(
            decompressor_scratch_bytes(&[0x0b, 0], CompressionType::Brotli, 1).unwrap(),
            STANDARD_WINDOW_SCRATCH_BYTES
        );
        assert_eq!(
            decompressor_scratch_bytes(&[0x11, 30], CompressionType::Brotli, 1).unwrap(),
            LARGE_WINDOW_SCRATCH_BYTES
        );
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
