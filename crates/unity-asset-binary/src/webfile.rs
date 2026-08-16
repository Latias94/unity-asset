//! Unity WebFile parsing
//!
//! WebFiles are Unity's web-optimized format that can contain other files
//! and may be compressed with gzip or brotli.

use crate::bundle::{AssetBundle, BundleFileInfo};
use crate::compression::{
    BrotliScratchReservation, CompressionType, DeferredByteReservation, SegmentedBrotliDecoder,
    decompress_brotli_with_budget, decompress_gzip_with_budget, decompressor_scratch_bytes,
};
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::random_access::{
    BorrowedBytes, ByteSource, ByteSourceReader, FallibleBufReader, SegmentedBytes,
};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
use flate2::bufread::GzDecoder;
use std::io::{self, Read};
use std::mem::size_of;
use std::ops::Range;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, BudgetError, DecompressionBudget,
    string_allocation_bytes, vec_allocation_bytes,
};

/// Magic bytes for different compression formats
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
// UnityPy uses the ASCII marker at offset 0x20 as a heuristic.
const BROTLI_MAGIC: &[u8] = b"brotli";
const INSPECTION_CODEC_BUFFER_SIZE: usize = 64 * 1024;

/// Compression type used in WebFile
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFileCompression {
    None,
    Gzip,
    Brotli,
}

/// Opaque proof produced by independently parsing and validating a WebFile image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFileInspection {
    signature: String,
    version: String,
    compression: WebFileCompression,
    head_length: u64,
    directory: Vec<WebFileDirectoryInspection>,
    stats: WebFileInspectionStats,
}

/// One WebFile directory record in wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFileDirectoryInspection {
    name: String,
    occurrence: usize,
    offset: u64,
    length: u64,
}

/// Bounded work performed while inspecting a WebFile image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebFileInspectionStats {
    encoded_bytes: u64,
    decoded_bytes: u64,
    metadata_bytes: u64,
    max_buffered_bytes: u64,
}

impl WebFileInspection {
    pub fn signature(&self) -> &str {
        &self.signature
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub const fn compression(&self) -> WebFileCompression {
        self.compression
    }

    pub const fn head_length(&self) -> u64 {
        self.head_length
    }

    pub fn directory(&self) -> &[WebFileDirectoryInspection] {
        &self.directory
    }

    pub const fn stats(&self) -> WebFileInspectionStats {
        self.stats
    }

    pub fn retained_heap_bytes(&self) -> Result<u64> {
        let mut bytes = string_allocation_bytes(self.signature.capacity())
            .map_err(webfile_inspection_allocation_error)?;
        webfile_add_retained(
            &mut bytes,
            string_allocation_bytes(self.version.capacity())
                .map_err(webfile_inspection_allocation_error)?,
        )?;
        webfile_add_retained(
            &mut bytes,
            vec_allocation_bytes::<WebFileDirectoryInspection>(self.directory.capacity())
                .map_err(webfile_inspection_allocation_error)?,
        )?;
        for entry in &self.directory {
            webfile_add_retained(
                &mut bytes,
                string_allocation_bytes(entry.name.capacity())
                    .map_err(webfile_inspection_allocation_error)?,
            )?;
        }
        Ok(bytes)
    }
}

impl WebFileDirectoryInspection {
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
}

impl WebFileInspectionStats {
    pub const fn encoded_bytes(self) -> u64 {
        self.encoded_bytes
    }

    pub const fn decoded_bytes(self) -> u64 {
        self.decoded_bytes
    }

    pub const fn metadata_bytes(self) -> u64 {
        self.metadata_bytes
    }

    pub const fn max_buffered_bytes(self) -> u64 {
        self.max_buffered_bytes
    }
}

fn webfile_inspection_allocation_error(
    error: unity_asset_core::AllocationSizeError,
) -> BinaryError {
    BinaryError::memory_error(format!(
        "WebFile inspection retained allocation size overflow: {error}"
    ))
}

fn webfile_add_retained(total: &mut u64, amount: u64) -> Result<()> {
    *total = total
        .checked_add(amount)
        .ok_or_else(|| BinaryError::memory_error("WebFile inspection retained heap overflow"))?;
    Ok(())
}

/// Staged WebFile probe failure.
///
/// A mismatch means the encoded stream or its decoded payload did not establish a WebFile
/// signature. Once a WebFile signature is recognized, every later parser failure is reported as
/// recognized corruption so callers cannot silently reinterpret malformed containers as raw data.
#[derive(Debug, Error)]
pub enum WebFileProbeError {
    #[error("input is not a recognized Unity WebFile: {source}")]
    Mismatch {
        #[source]
        source: BinaryError,
    },
    #[error("recognized Unity WebFile is malformed: {source}")]
    Recognized {
        #[source]
        source: BinaryError,
    },
}

impl WebFileProbeError {
    fn mismatch(source: BinaryError) -> Self {
        if source.is_resource_error() {
            Self::Recognized { source }
        } else {
            Self::Mismatch { source }
        }
    }

    fn recognized(source: BinaryError) -> Self {
        Self::Recognized { source }
    }

    /// Recovers the parser error used by the compatibility `from_*` APIs.
    #[must_use]
    pub fn into_source(self) -> BinaryError {
        match self {
            Self::Mismatch { source } | Self::Recognized { source } => source,
        }
    }
}

/// A Unity WebFile that can contain other files
#[derive(Debug)]
pub struct WebFile {
    /// Signature (e.g., "UnityWebData1.0")
    pub signature: String,
    /// Compression type used
    pub compression: WebFileCompression,
    /// Files contained in this WebFile
    pub files: Vec<BundleFileInfo>,
    /// Raw decompressed data
    data: DataView,
}

impl WebFile {
    /// Inspects a contiguous WebFile through the segmented streaming parser.
    pub fn inspect_slice_with_budget(
        data: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<WebFileInspection> {
        Self::inspect_source_with_budget(&BorrowedBytes::new(data), budget)
    }

    /// Inspects and validates a segmented WebFile without concatenating its encoded image.
    pub fn inspect_segmented_with_budget(
        data: &SegmentedBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<WebFileInspection> {
        Self::inspect_source_with_budget(data, budget)
    }

    fn inspect_source_with_budget(
        source: &dyn ByteSource,
        budget: &mut AssetLoadBudget,
    ) -> Result<WebFileInspection> {
        let compression = detect_source_compression(source)?;
        let compression =
            if compression == WebFileCompression::None && !source_has_webfile_prefix(source)? {
                WebFileCompression::Brotli
            } else {
                compression
            };

        match compression {
            WebFileCompression::None => inspect_uncompressed_webfile(source, budget),
            WebFileCompression::Gzip => inspect_gzip_webfile(source, budget),
            WebFileCompression::Brotli => inspect_brotli_webfile(source, budget),
        }
    }

    /// Parse a WebFile from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let mut budget = AssetLoadBudget::default();
        Self::from_bytes_with_budget(data, &mut budget)
    }

    /// Parse a WebFile while charging a caller-owned load budget.
    pub fn from_bytes_with_budget(data: Vec<u8>, budget: &mut AssetLoadBudget) -> Result<Self> {
        let shared = SharedBytes::from_vec(data);
        let len = shared.len();
        Self::from_shared_range_with_budget(shared, 0..len, budget)
    }

    pub fn from_shared_range(data: SharedBytes, range: Range<usize>) -> Result<Self> {
        let mut budget = AssetLoadBudget::default();
        Self::from_shared_range_with_budget(data, range, &mut budget)
    }

    pub fn from_shared_range_with_budget(
        data: SharedBytes,
        range: Range<usize>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        Self::probe_from_shared_range_with_budget(data, range, budget)
            .map_err(WebFileProbeError::into_source)
    }

    /// Probes and parses a shared byte range while preserving format-recognition state.
    ///
    /// Decompression and source accounting happen exactly once. Resource failures are always
    /// classified as [`WebFileProbeError::Recognized`] because retrying requires a larger caller
    /// budget rather than trying a different format.
    pub fn probe_from_shared_range_with_budget(
        data: SharedBytes,
        range: Range<usize>,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Self, WebFileProbeError> {
        let view =
            DataView::from_shared_range(data, range).map_err(WebFileProbeError::recognized)?;
        Self::probe_from_view_with_budget(view, budget)
    }

    fn probe_from_view_with_budget(
        view: DataView,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Self, WebFileProbeError> {
        let source_len = u64::try_from(view.len())
            .map_err(|_| BinaryError::invalid_data("WebFile source length does not fit in u64"))
            .map_err(WebFileProbeError::recognized)?;
        budget
            .consume_bytes(source_len)
            .map_err(BinaryError::from)
            .map_err(WebFileProbeError::recognized)?;

        // Detect compression with cheap heuristics first (UnityPy-style).
        let mut probe = BinaryReader::new(view.as_bytes(), ByteOrder::Little);
        let probed = Self::detect_compression(&mut probe).map_err(WebFileProbeError::mismatch)?;

        // Decompress if necessary, with a brotli fallback for non-heuristic streams.
        let (compression, decompressed_data, signature) = match probed {
            WebFileCompression::Gzip => {
                let decompressed = decompress_gzip_with_budget(view.as_bytes(), budget)
                    .map(SharedBytes::from_vec)
                    .map(DataView::from_shared)
                    .map_err(WebFileProbeError::mismatch)?;
                let signature = probe_webfile_signature(decompressed.as_bytes(), budget)?;
                (WebFileCompression::Gzip, decompressed, signature)
            }
            WebFileCompression::Brotli => {
                let decompressed = decompress_brotli_with_budget(view.as_bytes(), budget)
                    .map(SharedBytes::from_vec)
                    .map(DataView::from_shared)
                    .map_err(WebFileProbeError::mismatch)?;
                let signature = probe_webfile_signature(decompressed.as_bytes(), budget)?;
                (WebFileCompression::Brotli, decompressed, signature)
            }
            WebFileCompression::None => {
                // Attempt uncompressed parse first.
                match probe_webfile_signature(view.as_bytes(), budget) {
                    Ok(signature) => (WebFileCompression::None, view, signature),
                    Err(WebFileProbeError::Recognized { source }) => {
                        return Err(WebFileProbeError::Recognized { source });
                    }
                    Err(WebFileProbeError::Mismatch { .. }) => {
                        // Some brotli streams (including UnityPy's own WebFile.save output) do not
                        // match the 0x20 marker heuristic. Try brotli decompression as a fallback.
                        let decompressed = decompress_brotli_with_budget(view.as_bytes(), budget)
                            .map(SharedBytes::from_vec)
                            .map(DataView::from_shared)
                            .map_err(WebFileProbeError::mismatch)?;
                        let signature = probe_webfile_signature(decompressed.as_bytes(), budget)?;
                        (WebFileCompression::Brotli, decompressed, signature)
                    }
                }
            }
        };

        Self::parse_recognized_view_with_budget(compression, decompressed_data, signature, budget)
            .map_err(WebFileProbeError::recognized)
    }

    fn parse_recognized_view_with_budget(
        compression: WebFileCompression,
        decompressed_data: DataView,
        signature: String,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        let mut reader = BinaryReader::new(decompressed_data.as_bytes(), ByteOrder::Little);
        let signature_end = signature
            .len()
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("WebFile signature range overflow"))?;
        reader.set_position(u64::try_from(signature_end).map_err(|_| {
            BinaryError::invalid_data("WebFile signature end does not fit in u64")
        })?)?;

        // Read header length
        let head_length_i32 = reader.read_i32()?;
        if head_length_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative WebFile head_length: {}",
                head_length_i32
            )));
        }
        let head_length = head_length_i32 as usize;
        let total_len = decompressed_data.len();
        if head_length > total_len {
            return Err(BinaryError::invalid_data(format!(
                "WebFile head_length {} exceeds data len {}",
                head_length, total_len
            )));
        }
        if head_length < reader.position() as usize {
            return Err(BinaryError::invalid_data(format!(
                "WebFile head_length {} precedes current position {}",
                head_length,
                reader.position()
            )));
        }

        let directory_start = usize::try_from(reader.position()).map_err(|_| {
            BinaryError::invalid_data("WebFile directory start does not fit in usize")
        })?;
        let directory = preflight_webfile_directory(
            decompressed_data.as_bytes(),
            directory_start,
            head_length,
        )?;
        let entry_count = u64::try_from(directory.entry_count).map_err(|_| {
            BinaryError::invalid_data("WebFile directory entry count does not fit in u64")
        })?;
        budget.check_entries(entry_count)?;
        budget.check_members(entry_count)?;
        budget.check_bytes(directory.retained_bytes)?;
        budget.consume_entries(entry_count)?;
        budget.consume_members(entry_count)?;
        budget.consume_bytes(directory.retained_bytes)?;

        // Read file entries
        let mut files = Vec::new();
        files
            .try_reserve_exact(directory.entry_count)
            .map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {} WebFile directory entries: {error}",
                    directory.entry_count
                ))
            })?;
        while reader.position() < head_length as u64 {
            let fixed_end = reader
                .position()
                .checked_add(12)
                .ok_or_else(|| BinaryError::invalid_data("WebFile entry header overflow"))?;
            if fixed_end > head_length as u64 {
                return Err(BinaryError::invalid_data(
                    "WebFile entry header crosses head_length",
                ));
            }
            let offset_i32 = reader.read_i32()?;
            let length_i32 = reader.read_i32()?;
            let path_len_i32 = reader.read_i32()?;

            if offset_i32 < 0 || length_i32 < 0 || path_len_i32 < 0 {
                return Err(BinaryError::invalid_data(format!(
                    "Negative WebFile entry values: offset={} length={} path_len={}",
                    offset_i32, length_i32, path_len_i32
                )));
            }

            let offset = u64::try_from(offset_i32)
                .map_err(|_| BinaryError::invalid_data("Negative WebFile entry offset"))?;
            let length = u64::try_from(length_i32)
                .map_err(|_| BinaryError::invalid_data("Negative WebFile entry length"))?;
            let path_length = usize::try_from(path_len_i32)
                .map_err(|_| BinaryError::invalid_data("Negative WebFile entry path length"))?;
            if path_length > 16 * 1024 {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "WebFile entry name too large: {}",
                    path_length
                )));
            }
            let path_end = reader
                .position()
                .checked_add(u64::try_from(path_length).map_err(|_| {
                    BinaryError::invalid_data("WebFile entry path length does not fit in u64")
                })?)
                .ok_or_else(|| BinaryError::invalid_data("WebFile entry name range overflow"))?;
            if path_end > head_length as u64 {
                return Err(BinaryError::invalid_data(
                    "WebFile entry name crosses head_length",
                ));
            }
            let entry_end = offset
                .checked_add(length)
                .ok_or_else(|| BinaryError::invalid_data("WebFile entry data range overflow"))?;
            if offset < head_length as u64 || entry_end > total_len as u64 {
                return Err(BinaryError::invalid_data(format!(
                    "WebFile entry data range {offset}..{entry_end} is outside payload {head_length}..{total_len}"
                )));
            }
            let name_bytes = reader.read_bytes(path_length)?;
            let name = String::from_utf8(name_bytes).map_err(|e| {
                BinaryError::invalid_data(format!("Invalid UTF-8 in file name: {}", e))
            })?;

            files.push(BundleFileInfo {
                name,
                offset,
                size: length,
            });
        }
        if files.len() != directory.entry_count {
            return Err(BinaryError::invalid_data(
                "WebFile directory entry count changed after preflight",
            ));
        }

        Ok(WebFile {
            signature,
            compression,
            files,
            data: decompressed_data,
        })
    }

    /// Detect compression type from file header
    fn detect_compression(reader: &mut BinaryReader) -> Result<WebFileCompression> {
        // Check for GZIP magic
        if reader.len() >= GZIP_MAGIC.len() {
            let magic = reader.read_bytes(GZIP_MAGIC.len())?;
            reader.set_position(0)?;
            if magic == GZIP_MAGIC {
                return Ok(WebFileCompression::Gzip);
            }
        }

        // Check for Brotli magic at offset 0x20
        let brotli_end = 0x20_usize
            .checked_add(BROTLI_MAGIC.len())
            .ok_or_else(|| BinaryError::invalid_data("Brotli probe range overflow"))?;
        if reader.len() >= brotli_end {
            reader.set_position(0x20)?;
            let magic = reader.read_bytes(BROTLI_MAGIC.len())?;
            reader.set_position(0)?;
            if magic == BROTLI_MAGIC {
                return Ok(WebFileCompression::Brotli);
            }
        }

        Ok(WebFileCompression::None)
    }

    /// Get the files contained in this WebFile
    pub fn files(&self) -> &[BundleFileInfo] {
        &self.files
    }

    pub fn data_shared(&self) -> SharedBytes {
        self.data.backing_shared()
    }

    /// Extract a specific file by name
    pub fn extract_file(&self, name: &str) -> Result<Vec<u8>> {
        let mut budget = AssetLoadBudget::default();
        self.extract_file_with_budget(name, &mut budget)
    }

    /// Extract a file copy through a caller-owned cumulative load budget.
    pub fn extract_file_with_budget(
        &self,
        name: &str,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<u8>> {
        let bytes = self.extract_file_slice(name)?;
        let len = u64::try_from(bytes.len())
            .map_err(|_| BinaryError::invalid_data("WebFile entry size does not fit in u64"))?;
        budget.consume_bytes(len)?;
        let mut owned = Vec::new();
        owned.try_reserve_exact(bytes.len()).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {} extracted WebFile bytes: {error}",
                bytes.len()
            ))
        })?;
        owned.extend_from_slice(bytes);
        Ok(owned)
    }

    pub fn extract_file_slice(&self, name: &str) -> Result<&[u8]> {
        let file_info = self
            .files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| BinaryError::invalid_data(format!("File not found: {}", name)))?;
        let bytes = self.data.as_bytes();
        let range = checked_entry_range(file_info, bytes.len())?;
        Ok(&bytes[range])
    }

    pub fn extract_file_slice_by_info(&self, info: &BundleFileInfo) -> Result<&[u8]> {
        let bytes = self.data.as_bytes();
        let range = checked_entry_range(info, bytes.len())?;
        Ok(&bytes[range])
    }

    pub fn extract_file_view(&self, name: &str) -> Result<DataView> {
        let file_info = self
            .files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| BinaryError::invalid_data(format!("File not found: {}", name)))?;

        self.extract_file_view_by_info(file_info)
    }

    /// Extract a zero-copy view for one exact directory entry.
    pub fn extract_file_view_by_info(&self, info: &BundleFileInfo) -> Result<DataView> {
        let range = checked_entry_range(info, self.data.len())?;
        let base = self.data.base_offset();
        let absolute_start = base
            .checked_add(range.start)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry absolute start overflow"))?;
        let absolute_end = base
            .checked_add(range.end)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry absolute end overflow"))?;
        DataView::from_shared_range(self.data.backing_shared(), absolute_start..absolute_end)
    }

    /// Try to parse contained files as AssetBundles
    pub fn parse_bundles(&self) -> Result<Vec<AssetBundle>> {
        let mut budget = AssetLoadBudget::default();
        self.parse_bundles_with_budget(&mut budget)
    }

    /// Parse contained bundles through the same caller-owned load budget.
    pub fn parse_bundles_with_budget(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<AssetBundle>> {
        let mut bundles = Vec::new();

        for file_info in &self.files {
            let view = self.extract_file_view_by_info(file_info)?;
            if !crate::file::looks_like_bundle_prefix(view.as_bytes()) {
                continue;
            }

            match crate::bundle::BundleParser::from_shared_range_with_budget(
                view.backing_shared(),
                view.absolute_range(),
                budget,
            ) {
                Ok(bundle) => {
                    bundles.try_reserve(1).map_err(|error| {
                        BinaryError::memory_error(format!(
                            "Failed to reserve a parsed WebFile bundle: {error}"
                        ))
                    })?;
                    bundles.push(bundle);
                }
                Err(error) if error.is_resource_error() => return Err(error),
                Err(error) => {
                    return Err(BinaryError::parse_error(format!(
                        "Failed to parse AssetBundle from WebFile entry '{}': {error}",
                        file_info.name
                    )));
                }
            }
        }

        Ok(bundles)
    }
}

struct LocalInspectionBudget<'ledger> {
    limits: AssetLoadLimits,
    base: AssetLoadUsage,
    bytes: &'ledger DeferredByteReservation,
    entries: u64,
    members: u64,
}

impl<'ledger> LocalInspectionBudget<'ledger> {
    fn new(budget: &AssetLoadBudget, bytes: &'ledger DeferredByteReservation) -> Self {
        Self {
            limits: budget.limits(),
            base: budget.usage(),
            bytes,
            entries: 0,
            members: 0,
        }
    }

    fn reserve_bytes(&mut self, amount: u64) -> Result<()> {
        self.bytes.reserve_metadata(amount)
    }

    fn reserve_directory(&mut self, count: u64) -> Result<()> {
        self.entries = checked_local_charge(
            "entries",
            self.base.entries,
            self.entries,
            count,
            self.limits.max_entries,
        )?;
        self.members = checked_local_charge(
            "members",
            self.base.members,
            self.members,
            count,
            self.limits.max_members,
        )?;
        Ok(())
    }

    fn commit(self, budget: &mut AssetLoadBudget) -> Result<()> {
        budget.consume_entries(self.entries)?;
        budget.consume_members(self.members)?;
        Ok(())
    }
}

fn checked_local_charge(
    resource: &'static str,
    base: u64,
    used: u64,
    amount: u64,
    limit: u64,
) -> Result<u64> {
    let next =
        used.checked_add(amount)
            .ok_or(BinaryError::Budget(BudgetError::ArithmeticOverflow {
                resource,
            }))?;
    let requested =
        base.checked_add(next)
            .ok_or(BinaryError::Budget(BudgetError::ArithmeticOverflow {
                resource,
            }))?;
    if requested > limit {
        return Err(BinaryError::Budget(BudgetError::Exceeded {
            resource,
            limit,
            requested,
        }));
    }
    Ok(next)
}

struct BudgetedDecoder<'budget, R> {
    inner: R,
    budget: DecompressionBudget<'budget>,
}

impl<R: Read> Read for BudgetedDecoder<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(output)?;
        self.budget
            .consume(
                0,
                u64::try_from(read).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "decoded length does not fit u64",
                    )
                })?,
            )
            .map_err(|error| io::Error::other(BinaryError::Budget(error)))?;
        Ok(read)
    }
}

fn inspect_uncompressed_webfile(
    source: &dyn ByteSource,
    budget: &mut AssetLoadBudget,
) -> Result<WebFileInspection> {
    let bytes = DeferredByteReservation::new(budget);
    let mut local = LocalInspectionBudget::new(budget, &bytes);
    local.reserve_bytes(source.len())?;
    let result = {
        let mut reader = ByteSourceReader::new(source);
        inspect_decoded_webfile(
            &mut reader,
            WebFileCompression::None,
            source.len(),
            &mut local,
        )
    };
    finish_webfile_inspection(result, local, &bytes, budget)
}

fn inspect_gzip_webfile(
    source: &dyn ByteSource,
    budget: &mut AssetLoadBudget,
) -> Result<WebFileInspection> {
    let bytes = DeferredByteReservation::new(budget);
    let mut local = LocalInspectionBudget::new(budget, &bytes);
    local.reserve_bytes(source.len())?;
    bytes.reserve_scratch(
        u64::try_from(INSPECTION_CODEC_BUFFER_SIZE)
            .map_err(|_| BinaryError::invalid_data("GZIP input buffer size does not fit u64"))?,
    )?;
    let input = FallibleBufReader::try_with_capacity(
        INSPECTION_CODEC_BUFFER_SIZE,
        ByteSourceReader::new(source),
    );
    let result = input.and_then(|input| {
        let mut decompression = budget.begin_decompression();
        decompression.consume(source.len(), 0)?;
        let decoder = GzDecoder::new(input);
        let mut reader = BudgetedDecoder {
            inner: decoder,
            budget: decompression,
        };
        let mut result = inspect_decoded_webfile(
            &mut reader,
            WebFileCompression::Gzip,
            source.len(),
            &mut local,
        );
        if result.is_ok() {
            let input = reader.inner.get_ref();
            let physically_read = input.get_ref().bytes_read();
            let buffered = u64::try_from(input.buffer().len()).map_err(|_| {
                BinaryError::invalid_data("GZIP buffered input length does not fit u64")
            })?;
            let consumed = physically_read.checked_sub(buffered).ok_or_else(|| {
                BinaryError::invalid_data("GZIP input accounting moved backwards")
            })?;
            if consumed != source.len() {
                result = Err(BinaryError::decompression_failed(format!(
                    "GZIP stream contains {} trailing bytes after its single member",
                    source.len() - consumed
                )));
            }
        }
        result
    });
    let mut inspection = finish_webfile_inspection(result, local, &bytes, budget)?;
    inspection.stats.max_buffered_bytes = inspection
        .stats
        .max_buffered_bytes
        .checked_add(INSPECTION_CODEC_BUFFER_SIZE as u64)
        .ok_or_else(|| BinaryError::memory_error("GZIP inspection working set overflow"))?;
    Ok(inspection)
}

fn inspect_brotli_webfile(
    source: &dyn ByteSource,
    budget: &mut AssetLoadBudget,
) -> Result<WebFileInspection> {
    let mut prefix = [0_u8; 2];
    let prefix_len = usize::try_from(source.len().min(2))
        .map_err(|_| BinaryError::invalid_data("Brotli prefix length does not fit usize"))?;
    source.read_exact_at(0, &mut prefix[..prefix_len])?;
    let scratch = decompressor_scratch_bytes(&prefix[..prefix_len], CompressionType::Brotli, 0)?;
    let bytes = DeferredByteReservation::new(budget);
    let mut local = LocalInspectionBudget::new(budget, &bytes);
    local.reserve_bytes(source.len())?;
    let scratch_reservation = BrotliScratchReservation::with_deferred(&bytes);
    let result = {
        let mut decompression = budget.begin_decompression();
        decompression.consume(source.len(), 0)?;
        let decoder = SegmentedBrotliDecoder::new(source, &scratch_reservation);
        let mut reader = BudgetedDecoder {
            inner: decoder,
            budget: decompression,
        };
        inspect_decoded_webfile(
            &mut reader,
            WebFileCompression::Brotli,
            source.len(),
            &mut local,
        )
    };
    let result = match (scratch_reservation.finish(), result) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    };
    let mut inspection = finish_webfile_inspection(result, local, &bytes, budget)?;
    inspection.stats.max_buffered_bytes = inspection
        .stats
        .max_buffered_bytes
        .checked_add(scratch)
        .ok_or_else(|| BinaryError::memory_error("Brotli inspection working set overflow"))?;
    Ok(inspection)
}

fn finish_webfile_inspection(
    result: Result<WebFileInspection>,
    local: LocalInspectionBudget<'_>,
    bytes: &DeferredByteReservation,
    budget: &mut AssetLoadBudget,
) -> Result<WebFileInspection> {
    let metadata_bytes = bytes.metadata_bytes();
    let commit = local.commit(budget).and_then(|()| bytes.commit(budget));
    match (result, commit) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(mut inspection), Ok(())) => {
            inspection.stats.metadata_bytes = metadata_bytes;
            Ok(inspection)
        }
    }
}

fn inspect_decoded_webfile(
    reader: &mut impl Read,
    compression: WebFileCompression,
    encoded_bytes: u64,
    local: &mut LocalInspectionBudget,
) -> Result<WebFileInspection> {
    let signature = read_stream_cstring(reader, local)?;
    if !signature.starts_with("UnityWebData") && !signature.starts_with("TuanjieWebData") {
        return Err(BinaryError::invalid_signature(
            "UnityWebData or TuanjieWebData",
            &signature,
        ));
    }
    let version = signature
        .strip_prefix("UnityWebData")
        .or_else(|| signature.strip_prefix("TuanjieWebData"))
        .unwrap_or_default();
    local.reserve_bytes(
        u64::try_from(version.len())
            .map_err(|_| BinaryError::invalid_data("WebFile version length does not fit u64"))?,
    )?;
    let mut owned_version = String::new();
    owned_version
        .try_reserve_exact(version.len())
        .map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {} WebFile version bytes: {error}",
                version.len()
            ))
        })?;
    owned_version.push_str(version);

    let signature_wire_len = u64::try_from(signature.len())
        .map_err(|_| BinaryError::invalid_data("WebFile signature length does not fit u64"))?
        .checked_add(1)
        .ok_or_else(|| BinaryError::invalid_data("WebFile signature range overflow"))?;
    let head_length_i32 = read_stream_i32(reader, "WebFile head_length")?;
    if head_length_i32 < 0 {
        return Err(BinaryError::invalid_data(format!(
            "Negative WebFile head_length: {head_length_i32}"
        )));
    }
    let head_length = u64::try_from(head_length_i32)
        .map_err(|_| BinaryError::invalid_data("Negative WebFile head_length"))?;
    let directory_start = signature_wire_len
        .checked_add(4)
        .ok_or_else(|| BinaryError::invalid_data("WebFile directory start overflow"))?;
    if head_length < directory_start {
        return Err(BinaryError::invalid_data(format!(
            "WebFile head_length {head_length} precedes current position {directory_start}"
        )));
    }
    let directory_len = head_length - directory_start;
    local.reserve_bytes(directory_len)?;
    let directory_len_usize = usize::try_from(directory_len)
        .map_err(|_| BinaryError::memory_error("WebFile header does not fit usize"))?;
    let mut header = Vec::new();
    header
        .try_reserve_exact(directory_len_usize)
        .map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {directory_len_usize} WebFile header bytes: {error}"
            ))
        })?;
    header.resize(directory_len_usize, 0);
    read_stream_exact(reader, &mut header, "WebFile directory")?;

    let mut directory = parse_inspection_directory(&header, local)?;
    let mut decoded_bytes = head_length;
    let mut drain = [0_u8; 64 * 1024];
    loop {
        let read = read_stream(reader, &mut drain, "WebFile payload")?;
        if read == 0 {
            break;
        }
        decoded_bytes = decoded_bytes
            .checked_add(u64::try_from(read).map_err(|_| {
                BinaryError::invalid_data("WebFile decoded chunk length does not fit u64")
            })?)
            .ok_or_else(|| BinaryError::invalid_data("WebFile decoded length overflow"))?;
    }

    for entry in &directory {
        let end = entry
            .offset
            .checked_add(entry.length)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry data range overflow"))?;
        if entry.offset < head_length || end > decoded_bytes {
            return Err(BinaryError::invalid_data(format!(
                "WebFile entry data range {}..{end} is outside payload {head_length}..{decoded_bytes}",
                entry.offset
            )));
        }
    }
    assign_webfile_occurrences(&mut directory, local)?;

    Ok(WebFileInspection {
        signature,
        version: owned_version,
        compression,
        head_length,
        directory,
        stats: WebFileInspectionStats {
            encoded_bytes,
            decoded_bytes,
            metadata_bytes: 0,
            max_buffered_bytes: directory_len,
        },
    })
}

fn read_stream_cstring(
    reader: &mut impl Read,
    local: &mut LocalInspectionBudget,
) -> Result<String> {
    let max_len = BinaryReader::DEFAULT_MAX_STRING_LEN;
    let mut bytes = Vec::new();
    while bytes.len() <= max_len {
        let mut byte = [0_u8; 1];
        read_stream_exact(reader, &mut byte, "WebFile signature")?;
        if byte[0] == 0 {
            return String::from_utf8(bytes).map_err(Into::into);
        }
        if bytes.len() == max_len {
            return Err(BinaryError::invalid_data(format!(
                "WebFile signature exceeds maximum length {max_len}"
            )));
        }
        if bytes.len() == bytes.capacity() {
            let next_capacity = if bytes.capacity() == 0 {
                64.min(max_len)
            } else {
                bytes.capacity().saturating_mul(2).min(max_len)
            };
            let additional = next_capacity.saturating_sub(bytes.capacity());
            local.reserve_bytes(u64::try_from(additional).map_err(|_| {
                BinaryError::invalid_data("WebFile signature capacity does not fit u64")
            })?)?;
            bytes.try_reserve_exact(additional).map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to grow WebFile signature to {next_capacity} bytes: {error}"
                ))
            })?;
        }
        bytes.push(byte[0]);
    }
    Err(BinaryError::invalid_data(
        "WebFile signature exceeded its parser limit",
    ))
}

fn read_stream_i32(reader: &mut impl Read, context: &'static str) -> Result<i32> {
    let mut bytes = [0_u8; 4];
    read_stream_exact(reader, &mut bytes, context)?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_stream_exact(
    reader: &mut impl Read,
    output: &mut [u8],
    context: &'static str,
) -> Result<()> {
    reader
        .read_exact(output)
        .map_err(|error| map_stream_error(error, context))
}

fn read_stream(reader: &mut impl Read, output: &mut [u8], context: &'static str) -> Result<usize> {
    reader
        .read(output)
        .map_err(|error| map_stream_error(error, context))
}

fn map_stream_error(error: io::Error, context: &'static str) -> BinaryError {
    let kind = error.kind();
    let message = error.to_string();
    if let Some(source) = error.into_inner()
        && let Ok(binary) = source.downcast::<BinaryError>()
    {
        return *binary;
    }
    if kind == io::ErrorKind::UnexpectedEof {
        BinaryError::invalid_data(format!("Unexpected end of decoded {context}"))
    } else {
        BinaryError::decompression_failed(format!("Failed to read decoded {context}: {message}"))
    }
}

fn parse_inspection_directory(
    header: &[u8],
    local: &mut LocalInspectionBudget,
) -> Result<Vec<WebFileDirectoryInspection>> {
    let mut cursor = 0_usize;
    let mut entry_count = 0_usize;
    let mut name_bytes = 0_usize;
    while cursor < header.len() {
        let fixed_end = cursor
            .checked_add(12)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry header overflow"))?;
        let fixed = header
            .get(cursor..fixed_end)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry header crosses head_length"))?;
        let offset = i32::from_le_bytes(
            fixed[0..4]
                .try_into()
                .map_err(|_| BinaryError::invalid_data("Invalid WebFile entry offset width"))?,
        );
        let length = i32::from_le_bytes(
            fixed[4..8]
                .try_into()
                .map_err(|_| BinaryError::invalid_data("Invalid WebFile entry length width"))?,
        );
        let path_len =
            i32::from_le_bytes(fixed[8..12].try_into().map_err(|_| {
                BinaryError::invalid_data("Invalid WebFile entry path length width")
            })?);
        if offset < 0 || length < 0 || path_len < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative WebFile entry values: offset={offset} length={length} path_len={path_len}"
            )));
        }
        let path_len = usize::try_from(path_len)
            .map_err(|_| BinaryError::invalid_data("Negative WebFile path length"))?;
        if path_len > 16 * 1024 {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "WebFile entry name too large: {path_len}"
            )));
        }
        let path_end = fixed_end
            .checked_add(path_len)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry name range overflow"))?;
        let path = header
            .get(fixed_end..path_end)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry name crosses head_length"))?;
        std::str::from_utf8(path)?;
        name_bytes = name_bytes
            .checked_add(path_len)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry name total overflow"))?;
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry count overflow"))?;
        cursor = path_end;
    }

    let count = u64::try_from(entry_count)
        .map_err(|_| BinaryError::invalid_data("WebFile entry count does not fit u64"))?;
    local.reserve_directory(count)?;
    let table_bytes = size_of::<WebFileDirectoryInspection>()
        .checked_mul(entry_count)
        .and_then(|bytes| bytes.checked_add(name_bytes))
        .ok_or_else(|| BinaryError::invalid_data("WebFile inspection directory size overflow"))?;
    local.reserve_bytes(u64::try_from(table_bytes).map_err(|_| {
        BinaryError::invalid_data("WebFile inspection directory size does not fit u64")
    })?)?;

    let mut directory = Vec::new();
    directory.try_reserve_exact(entry_count).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {entry_count} WebFile inspection records: {error}"
        ))
    })?;
    cursor = 0;
    while cursor < header.len() {
        let fixed_end = cursor + 12;
        let fixed = &header[cursor..fixed_end];
        let offset =
            u64::try_from(i32::from_le_bytes(fixed[0..4].try_into().map_err(
                |_| BinaryError::invalid_data("Invalid WebFile entry offset width"),
            )?))
            .map_err(|_| BinaryError::invalid_data("Negative WebFile entry offset"))?;
        let length =
            u64::try_from(i32::from_le_bytes(fixed[4..8].try_into().map_err(
                |_| BinaryError::invalid_data("Invalid WebFile entry length width"),
            )?))
            .map_err(|_| BinaryError::invalid_data("Negative WebFile entry length"))?;
        let path_len =
            usize::try_from(i32::from_le_bytes(fixed[8..12].try_into().map_err(
                |_| BinaryError::invalid_data("Invalid WebFile path length width"),
            )?))
            .map_err(|_| BinaryError::invalid_data("Negative WebFile path length"))?;
        let path_end = fixed_end + path_len;
        let path = std::str::from_utf8(&header[fixed_end..path_end])?;
        let mut name = String::new();
        name.try_reserve_exact(path_len).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {path_len} WebFile entry name bytes: {error}"
            ))
        })?;
        name.push_str(path);
        directory.push(WebFileDirectoryInspection {
            name,
            occurrence: 0,
            offset,
            length,
        });
        cursor = path_end;
    }
    Ok(directory)
}

fn assign_webfile_occurrences(
    directory: &mut [WebFileDirectoryInspection],
    local: &mut LocalInspectionBudget,
) -> Result<()> {
    let index_bytes = size_of::<usize>()
        .checked_mul(directory.len())
        .ok_or_else(|| BinaryError::invalid_data("WebFile occurrence state size overflow"))?;
    local.reserve_bytes(u64::try_from(index_bytes).map_err(|_| {
        BinaryError::invalid_data("WebFile occurrence state size does not fit u64")
    })?)?;
    let mut sorted = Vec::new();
    sorted.try_reserve_exact(directory.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {} WebFile occurrence indices: {error}",
            directory.len()
        ))
    })?;
    sorted.extend(0..directory.len());
    sorted.sort_unstable_by(|left, right| {
        directory[*left]
            .name
            .cmp(&directory[*right].name)
            .then_with(|| left.cmp(right))
    });
    let mut previous_index: Option<usize> = None;
    let mut occurrence = 0_usize;
    for index in sorted {
        if previous_index.is_some_and(|previous| directory[previous].name == directory[index].name)
        {
            occurrence = occurrence
                .checked_add(1)
                .ok_or_else(|| BinaryError::invalid_data("WebFile occurrence overflow"))?;
        } else {
            occurrence = 0;
        }
        directory[index].occurrence = occurrence;
        previous_index = Some(index);
    }
    Ok(())
}

fn detect_source_compression(source: &dyn ByteSource) -> Result<WebFileCompression> {
    const PROBE_LEN: usize = 0x20 + BROTLI_MAGIC.len();
    let read_len = usize::try_from(source.len().min(PROBE_LEN as u64))
        .map_err(|_| BinaryError::invalid_data("WebFile probe length does not fit usize"))?;
    let mut probe = [0_u8; PROBE_LEN];
    source.read_exact_at(0, &mut probe[..read_len])?;
    if probe[..read_len].starts_with(GZIP_MAGIC) {
        return Ok(WebFileCompression::Gzip);
    }
    if read_len >= PROBE_LEN && &probe[0x20..PROBE_LEN] == BROTLI_MAGIC {
        return Ok(WebFileCompression::Brotli);
    }
    Ok(WebFileCompression::None)
}

fn source_has_webfile_prefix(source: &dyn ByteSource) -> Result<bool> {
    const PREFIX_LEN: usize = b"TuanjieWebData".len();
    let read_len = usize::try_from(source.len().min(PREFIX_LEN as u64))
        .map_err(|_| BinaryError::invalid_data("WebFile prefix length does not fit usize"))?;
    let mut prefix = [0_u8; PREFIX_LEN];
    source.read_exact_at(0, &mut prefix[..read_len])?;
    Ok(prefix[..read_len].starts_with(b"UnityWebData")
        || prefix[..read_len].starts_with(b"TuanjieWebData"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WebFileDirectoryPreflight {
    entry_count: usize,
    retained_bytes: u64,
}

fn preflight_webfile_directory(
    data: &[u8],
    directory_start: usize,
    head_length: usize,
) -> Result<WebFileDirectoryPreflight> {
    if directory_start > head_length || head_length > data.len() {
        return Err(BinaryError::invalid_data(
            "WebFile directory range is outside the decompressed image",
        ));
    }

    let mut cursor = directory_start;
    let mut entry_count = 0_usize;
    let mut name_bytes = 0_usize;
    while cursor < head_length {
        let fixed_end = cursor
            .checked_add(12)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry header overflow"))?;
        if fixed_end > head_length {
            return Err(BinaryError::invalid_data(
                "WebFile entry header crosses head_length",
            ));
        }

        let offset_i32 = webfile_i32_at(data, cursor, "entry offset")?;
        let length_i32 = webfile_i32_at(data, cursor + 4, "entry length")?;
        let path_len_i32 = webfile_i32_at(data, cursor + 8, "entry path length")?;
        if offset_i32 < 0 || length_i32 < 0 || path_len_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative WebFile entry values: offset={} length={} path_len={}",
                offset_i32, length_i32, path_len_i32
            )));
        }

        let offset = u64::try_from(offset_i32)
            .map_err(|_| BinaryError::invalid_data("Negative WebFile entry offset"))?;
        let length = u64::try_from(length_i32)
            .map_err(|_| BinaryError::invalid_data("Negative WebFile entry length"))?;
        let path_length = usize::try_from(path_len_i32)
            .map_err(|_| BinaryError::invalid_data("Negative WebFile entry path length"))?;
        if path_length > 16 * 1024 {
            return Err(BinaryError::ResourceLimitExceeded(format!(
                "WebFile entry name too large: {}",
                path_length
            )));
        }

        let path_end = fixed_end
            .checked_add(path_length)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry name range overflow"))?;
        if path_end > head_length {
            return Err(BinaryError::invalid_data(
                "WebFile entry name crosses head_length",
            ));
        }
        let entry_end = offset
            .checked_add(length)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry data range overflow"))?;
        if offset < head_length as u64 || entry_end > data.len() as u64 {
            return Err(BinaryError::invalid_data(format!(
                "WebFile entry data range {offset}..{entry_end} is outside payload {head_length}..{}",
                data.len()
            )));
        }
        let name = data
            .get(fixed_end..path_end)
            .ok_or_else(|| BinaryError::not_enough_data(path_end, data.len()))?;
        std::str::from_utf8(name).map_err(|error| {
            BinaryError::invalid_data(format!("Invalid UTF-8 in file name: {error}"))
        })?;

        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry count overflow"))?;
        name_bytes = name_bytes
            .checked_add(path_length)
            .ok_or_else(|| BinaryError::invalid_data("WebFile entry name byte total overflow"))?;
        cursor = path_end;
    }

    let table_bytes = size_of::<BundleFileInfo>()
        .checked_mul(entry_count)
        .ok_or_else(|| BinaryError::invalid_data("WebFile directory table size overflow"))?;
    let retained_bytes = table_bytes
        .checked_add(name_bytes)
        .ok_or_else(|| BinaryError::invalid_data("WebFile retained directory size overflow"))?;
    Ok(WebFileDirectoryPreflight {
        entry_count,
        retained_bytes: u64::try_from(retained_bytes).map_err(|_| {
            BinaryError::invalid_data("WebFile retained directory size does not fit in u64")
        })?,
    })
}

fn webfile_i32_at(data: &[u8], offset: usize, field: &'static str) -> Result<i32> {
    let end = offset
        .checked_add(size_of::<i32>())
        .ok_or_else(|| BinaryError::invalid_data(format!("WebFile {field} range overflow")))?;
    let bytes: [u8; 4] = data
        .get(offset..end)
        .ok_or_else(|| BinaryError::not_enough_data(end, data.len()))?
        .try_into()
        .map_err(|_| BinaryError::invalid_data(format!("Invalid WebFile {field} width")))?;
    Ok(i32::from_le_bytes(bytes))
}

fn checked_entry_range(info: &BundleFileInfo, total_len: usize) -> Result<Range<usize>> {
    let start = usize::try_from(info.offset).map_err(|_| {
        BinaryError::invalid_data(format!(
            "File {} offset {} does not fit in usize",
            info.name, info.offset
        ))
    })?;
    let size = usize::try_from(info.size).map_err(|_| {
        BinaryError::invalid_data(format!(
            "File {} size {} does not fit in usize",
            info.name, info.size
        ))
    })?;
    let end = start
        .checked_add(size)
        .ok_or_else(|| BinaryError::invalid_data("WebFile entry offset+size overflow"))?;
    if end > total_len {
        return Err(BinaryError::invalid_data(format!(
            "File {} extends beyond data bounds: {} > {}",
            info.name, end, total_len
        )));
    }
    Ok(start..end)
}

fn probe_webfile_signature(
    data: &[u8],
    budget: &mut AssetLoadBudget,
) -> std::result::Result<String, WebFileProbeError> {
    let has_recognized_prefix = has_webfile_signature_prefix(data);
    read_webfile_signature_with_budget(data, budget).map_err(|source| {
        if has_recognized_prefix || source.is_resource_error() {
            WebFileProbeError::recognized(source)
        } else {
            WebFileProbeError::mismatch(source)
        }
    })
}

fn read_webfile_signature_with_budget(data: &[u8], budget: &mut AssetLoadBudget) -> Result<String> {
    let max_len = BinaryReader::DEFAULT_MAX_STRING_LEN;
    let scan_len = data.len().min(max_len.saturating_add(1));
    let signature_end = data[..scan_len]
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| {
            if data.len() > max_len {
                BinaryError::invalid_data(format!(
                    "WebFile signature exceeds maximum length {max_len}"
                ))
            } else {
                BinaryError::invalid_data("unterminated WebFile signature")
            }
        })?;
    let signature = std::str::from_utf8(&data[..signature_end])?;
    if !signature.starts_with("UnityWebData") && !signature.starts_with("TuanjieWebData") {
        return Err(BinaryError::invalid_signature(
            "UnityWebData or TuanjieWebData",
            signature,
        ));
    }

    let retained_bytes = u64::try_from(signature.len())
        .map_err(|_| BinaryError::invalid_data("WebFile signature length does not fit in u64"))?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_bytes(retained_bytes)?;
    let mut owned = String::new();
    owned.try_reserve_exact(signature.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {} WebFile signature bytes: {error}",
            signature.len()
        ))
    })?;
    owned.push_str(signature);
    Ok(owned)
}

fn has_webfile_signature_prefix(data: &[u8]) -> bool {
    data.starts_with(b"UnityWebData") || data.starts_with(b"TuanjieWebData")
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{AssetLoadLimits, BudgetError, arc_vec_allocation_bytes};

    fn minimal_webfile(head_length: i32, header_tail: &[u8]) -> Vec<u8> {
        let mut bytes = b"UnityWebData1.0\0".to_vec();
        bytes.extend_from_slice(&head_length.to_le_bytes());
        bytes.extend_from_slice(header_tail);
        bytes
    }

    fn webfile_with_entry(name: &str, payload: &[u8]) -> Vec<u8> {
        let head_length = 20_usize + 12 + name.len();
        let mut entry = Vec::new();
        entry.extend_from_slice(
            &i32::try_from(head_length)
                .expect("test WebFile header fits in i32")
                .to_le_bytes(),
        );
        entry.extend_from_slice(
            &i32::try_from(payload.len())
                .expect("test WebFile payload fits in i32")
                .to_le_bytes(),
        );
        entry.extend_from_slice(
            &i32::try_from(name.len())
                .expect("test WebFile name fits in i32")
                .to_le_bytes(),
        );
        entry.extend_from_slice(name.as_bytes());
        let mut bytes = minimal_webfile(
            i32::try_from(head_length).expect("test WebFile header fits in i32"),
            &entry,
        );
        bytes.extend_from_slice(payload);
        bytes
    }

    fn webfile_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let head_length = entries.iter().fold(20_usize, |length, (name, _)| {
            length
                .checked_add(12 + name.len())
                .expect("test WebFile header length does not overflow")
        });
        let mut payload_offset = head_length;
        let mut directory = Vec::new();

        for (name, payload) in entries {
            directory.extend_from_slice(
                &i32::try_from(payload_offset)
                    .expect("test WebFile payload offset fits in i32")
                    .to_le_bytes(),
            );
            directory.extend_from_slice(
                &i32::try_from(payload.len())
                    .expect("test WebFile payload length fits in i32")
                    .to_le_bytes(),
            );
            directory.extend_from_slice(
                &i32::try_from(name.len())
                    .expect("test WebFile name length fits in i32")
                    .to_le_bytes(),
            );
            directory.extend_from_slice(name.as_bytes());
            payload_offset = payload_offset
                .checked_add(payload.len())
                .expect("test WebFile payload range does not overflow");
        }

        let mut bytes = minimal_webfile(
            i32::try_from(head_length).expect("test WebFile header fits in i32"),
            &directory,
        );
        for (_, payload) in entries {
            bytes.extend_from_slice(payload);
        }
        bytes
    }

    fn retained_directory_bytes(names: &[&str]) -> u64 {
        u64::try_from(
            names.len() * std::mem::size_of::<BundleFileInfo>()
                + names.iter().map(|name| name.len()).sum::<usize>(),
        )
        .expect("test WebFile retained directory size fits in u64")
    }

    fn retained_signature_bytes() -> u64 {
        u64::try_from("UnityWebData1.0".len()).expect("test WebFile signature size fits in u64")
    }

    fn gzip_compress(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        std::io::Write::write_all(&mut encoder, bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn webfile_with_corrupt_directory() -> Vec<u8> {
        let mut entry = Vec::new();
        entry.extend_from_slice(&33_i32.to_le_bytes());
        entry.extend_from_slice(&0_i32.to_le_bytes());
        entry.extend_from_slice(&4_i32.to_le_bytes());
        entry.push(b'x');
        minimal_webfile(33, &entry)
    }

    fn minimal_unityfs_bundle(revision: &str) -> Vec<u8> {
        let mut blocks_info = vec![0_u8; 16];
        blocks_info.extend_from_slice(&1_i32.to_be_bytes());
        blocks_info.extend_from_slice(&1_u32.to_be_bytes());
        blocks_info.extend_from_slice(&1_u32.to_be_bytes());
        blocks_info.extend_from_slice(&0_u16.to_be_bytes());
        blocks_info.extend_from_slice(&0_i32.to_be_bytes());

        let mut bytes = b"UnityFS\0".to_vec();
        bytes.extend_from_slice(&7_u32.to_be_bytes());
        bytes.extend_from_slice(b"5.x.x\0");
        bytes.extend_from_slice(revision.as_bytes());
        bytes.push(0);
        let size_offset = bytes.len();
        bytes.extend_from_slice(&0_i64.to_be_bytes());
        bytes.extend_from_slice(
            &u32::try_from(blocks_info.len())
                .expect("test blocks info length fits in u32")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(
            &u32::try_from(blocks_info.len())
                .expect("test blocks info length fits in u32")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        while !bytes.len().is_multiple_of(16) {
            bytes.push(0);
        }
        bytes.extend_from_slice(&blocks_info);
        bytes.push(0);
        let total_len = i64::try_from(bytes.len()).expect("test bundle length fits in i64");
        bytes[size_offset..size_offset + 8].copy_from_slice(&total_len.to_be_bytes());
        bytes
    }

    #[test]
    fn test_compression_detection() {
        // Test GZIP magic detection
        let gzip_data = [0x1f, 0x8b, 0x08, 0x00];
        let mut reader = BinaryReader::new(&gzip_data, ByteOrder::Little);
        let compression = WebFile::detect_compression(&mut reader).unwrap();
        assert_eq!(compression, WebFileCompression::Gzip);
    }

    #[test]
    fn compressed_recognized_corruption_is_not_a_probe_mismatch() {
        for decoded in [
            minimal_webfile(-1, &[]),
            minimal_webfile(1024, &[]),
            webfile_with_corrupt_directory(),
        ] {
            let encoded = gzip_compress(&decoded);
            let shared = SharedBytes::from_vec(encoded);
            let len = shared.len();
            let mut budget = AssetLoadBudget::default();

            let error = WebFile::probe_from_shared_range_with_budget(shared, 0..len, &mut budget)
                .expect_err("recognized corrupt WebFile must fail its parse");

            assert!(matches!(
                error,
                WebFileProbeError::Recognized {
                    source: BinaryError::InvalidData(_),
                }
            ));
        }
    }

    #[test]
    fn compressed_non_webfile_payload_is_a_probe_mismatch() {
        let decoded = b"ordinary gzip payload";
        let encoded = gzip_compress(decoded);
        let encoded_len = u64::try_from(encoded.len()).unwrap();
        let shared = SharedBytes::from_vec(encoded);
        let len = shared.len();
        let mut budget = AssetLoadBudget::default();

        let error = WebFile::probe_from_shared_range_with_budget(shared, 0..len, &mut budget)
            .expect_err("gzip alone does not establish a WebFile");

        assert!(matches!(error, WebFileProbeError::Mismatch { .. }));
        assert_eq!(budget.usage().compressed_bytes, encoded_len);
        assert_eq!(
            budget.usage().decompressed_bytes,
            u64::try_from(decoded.len()).unwrap()
        );
    }

    #[test]
    fn probe_resource_failure_is_always_recognized() {
        let encoded = gzip_compress(b"ordinary gzip payload");
        let source_len = u64::try_from(encoded.len()).unwrap();
        let shared = SharedBytes::from_vec(encoded);
        let len = shared.len();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: source_len - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = WebFile::probe_from_shared_range_with_budget(shared, 0..len, &mut budget)
            .expect_err("resource failure cannot prove a format mismatch");

        assert!(matches!(
            error,
            WebFileProbeError::Recognized {
                source: BinaryError::Budget(_),
            }
        ));
    }

    #[test]
    fn test_webfile_creation() {
        // Test basic WebFile structure creation
        let data = DataView::from_shared(SharedBytes::from_vec(Vec::<u8>::new()));
        let webfile = WebFile {
            signature: "UnityWebData1.0".to_string(),
            compression: WebFileCompression::None,
            files: Vec::new(),
            data,
        };

        assert_eq!(webfile.signature, "UnityWebData1.0");
        assert_eq!(webfile.compression, WebFileCompression::None);
        assert!(webfile.files().is_empty());
    }

    #[test]
    fn short_uncompressed_webfile_does_not_require_brotli_probe_bytes() {
        let bytes = minimal_webfile(20, &[]);
        let webfile = WebFile::from_bytes(bytes).expect("short WebFile header is valid");

        assert_eq!(webfile.compression, WebFileCompression::None);
        assert!(webfile.files.is_empty());
    }

    #[test]
    fn entry_name_must_fit_entirely_inside_header() {
        let mut entry = Vec::new();
        entry.extend_from_slice(&32_i32.to_le_bytes());
        entry.extend_from_slice(&4_i32.to_le_bytes());
        entry.extend_from_slice(&4_i32.to_le_bytes());
        entry.extend_from_slice(b"n");
        let mut bytes = minimal_webfile(33, &entry);
        bytes.extend_from_slice(b"ame!");

        let error = WebFile::from_bytes(bytes).expect_err("entry name crosses head_length");
        assert!(error.to_string().contains("WebFile entry"));
    }

    #[test]
    fn directory_entries_consume_the_caller_owned_budget() {
        let mut directory = Vec::new();
        for (offset, name) in [(46_i32, b'a'), (47_i32, b'b')] {
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(&1_i32.to_le_bytes());
            directory.extend_from_slice(&1_i32.to_le_bytes());
            directory.push(name);
        }
        let mut bytes = minimal_webfile(46, &directory);
        bytes.extend_from_slice(b"xy");
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..Default::default()
        })
        .unwrap();

        let error = WebFile::from_bytes_with_budget(bytes, &mut budget).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn member_limit_precedes_webfile_directory_allocations() {
        let bytes = webfile_with_entries(&[("left", b"a"), ("right", b"b")]);
        let source_len = u64::try_from(bytes.len()).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = WebFile::from_bytes_with_budget(bytes, &mut budget).unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "members",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(
            budget.usage().bytes,
            source_len + retained_signature_bytes()
        );
    }

    #[test]
    fn compressed_member_preflight_decompresses_exactly_once() {
        let decoded = webfile_with_entries(&[("left", b"a"), ("right", b"b")]);
        let encoded = gzip_compress(&decoded);
        let encoded_len = u64::try_from(encoded.len()).unwrap();
        let decoded_len = u64::try_from(decoded.len()).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = WebFile::from_bytes_with_budget(encoded, &mut budget).unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "members",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(budget.usage().compressed_bytes, encoded_len);
        assert_eq!(budget.usage().decompressed_bytes, decoded_len);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn retained_directory_bytes_are_preflighted_before_allocation() {
        let entries = [("left", b"a".as_slice()), ("right", b"b".as_slice())];
        let bytes = webfile_with_entries(&entries);
        let source_len = u64::try_from(bytes.len()).unwrap();
        let retained = retained_directory_bytes(&["left", "right"]);
        let retained_signature = retained_signature_bytes();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: source_len + retained_signature + retained - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = WebFile::from_bytes_with_budget(bytes, &mut budget).unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == source_len + retained_signature + retained - 1
                && requested == source_len + retained_signature + retained
        ));
        assert_eq!(budget.usage().bytes, source_len + retained_signature);
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn encoded_source_is_rejected_before_parsing_when_byte_budget_is_too_small() {
        let bytes = minimal_webfile(20, &[]);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(bytes.len() - 1).unwrap(),
            ..Default::default()
        })
        .unwrap();

        let error = WebFile::from_bytes_with_budget(bytes, &mut budget).unwrap_err();
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
    fn nested_bundle_parsing_reuses_the_webfile_budget() {
        let bundle_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/samples/char_118_yuki.ab");
        let bundle_bytes = std::fs::read(bundle_path).expect("read sample bundle");
        let webfile_bytes = webfile_with_entry("sample.bundle", &bundle_bytes);
        let outer_cost = u64::try_from(webfile_bytes.len()).expect("test WebFile size fits in u64")
            + retained_signature_bytes()
            + retained_directory_bytes(&["sample.bundle"]);
        let requested =
            outer_cost + u64::try_from(bundle_bytes.len()).expect("test bundle size fits in u64");
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: requested - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let webfile = WebFile::from_bytes_with_budget(webfile_bytes.clone(), &mut budget)
            .expect("outer WebFile fits the byte budget");

        let error = webfile.parse_bundles_with_budget(&mut budget).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested: actual,
            }) if limit == requested - 1 && actual == requested
        ));
        assert_eq!(budget.usage().bytes, outer_cost);
    }

    #[test]
    fn duplicate_names_preserve_directory_identity_when_parsing_bundles() {
        let samples = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/samples");
        let first = std::fs::read(samples.join("atlas_test")).expect("read first sample bundle");
        let second = std::fs::read(samples.join("banner_1")).expect("read second sample bundle");
        let bytes = webfile_with_entries(&[
            ("duplicate.bundle", first.as_slice()),
            ("duplicate.bundle", second.as_slice()),
        ]);
        let webfile = WebFile::from_bytes(bytes).expect("parse test WebFile");

        assert_ne!(webfile.files[0].offset, webfile.files[1].offset);
        let bundles = webfile
            .parse_bundles()
            .expect("parse both duplicate-named bundle entries");
        let revisions = bundles
            .iter()
            .map(|bundle| bundle.header.unity_revision.as_str())
            .collect::<Vec<_>>();

        assert_eq!(revisions, ["2018.4.11f1", "2018.4.4f1"]);
    }

    #[test]
    fn non_bundle_entry_is_skipped_without_charging_the_nested_budget() {
        let webfile = WebFile::from_bytes(webfile_with_entry(
            "ordinary-data.bin",
            b"ordinary non-bundle payload",
        ))
        .expect("parse test WebFile");
        let mut budget = AssetLoadBudget::default();

        let bundles = webfile
            .parse_bundles_with_budget(&mut budget)
            .expect("non-bundle entries are skipped");

        assert!(bundles.is_empty());
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn corrupt_recognized_bundle_entries_report_the_entry_name() {
        for (name, payload) in [
            ("corrupt-unityfs.bundle", b"UnityFS\0".as_slice()),
            ("corrupt-unityraw.bundle", b"UnityRaw\0".as_slice()),
        ] {
            let webfile =
                WebFile::from_bytes(webfile_with_entry(name, payload)).expect("parse test WebFile");

            let error = webfile
                .parse_bundles()
                .expect_err("recognized corrupt bundle must fail the WebFile parse");

            assert!(matches!(&error, BinaryError::ParseError(_)));
            assert!(
                error.to_string().contains(name),
                "error must identify the corrupt WebFile entry: {error}"
            );
        }
    }

    #[test]
    fn recognized_valid_bundle_entry_is_parsed() {
        let bundle = minimal_unityfs_bundle("2020.3.0f1");
        let webfile = WebFile::from_bytes(webfile_with_entry("valid.bundle", &bundle))
            .expect("parse test WebFile");

        let bundles = webfile
            .parse_bundles()
            .expect("recognized valid bundle is parsed");

        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].header.unity_revision, "2020.3.0f1");
    }

    #[test]
    fn duplicate_entry_views_preserve_backing_offset_and_content() {
        let webfile_bytes =
            webfile_with_entries(&[("duplicate.bin", b"first"), ("duplicate.bin", b"second")]);
        let prefix_len = 7;
        let mut backing = vec![0xAA; prefix_len];
        let webfile_start = backing.len();
        backing.extend_from_slice(&webfile_bytes);
        let webfile_end = backing.len();
        let webfile =
            WebFile::from_shared_range(SharedBytes::from_vec(backing), webfile_start..webfile_end)
                .expect("parse WebFile from a non-zero backing offset");

        let first = webfile
            .extract_file_view_by_info(&webfile.files[0])
            .expect("extract first duplicate entry");
        let second = webfile
            .extract_file_view_by_info(&webfile.files[1])
            .expect("extract second duplicate entry");

        assert_eq!(first.as_bytes(), b"first");
        assert_eq!(second.as_bytes(), b"second");
        assert_eq!(
            first.base_offset(),
            webfile_start + usize::try_from(webfile.files[0].offset).unwrap()
        );
        assert_eq!(
            second.base_offset(),
            webfile_start + usize::try_from(webfile.files[1].offset).unwrap()
        );
        assert_eq!(
            webfile
                .extract_file_view("duplicate.bin")
                .expect("name lookup keeps first-match semantics")
                .as_bytes(),
            b"first"
        );
    }

    #[test]
    fn entry_view_rejects_an_out_of_bounds_directory_record() {
        let webfile = WebFile::from_bytes(webfile_with_entry("payload.bin", b"data"))
            .expect("parse test WebFile");
        let invalid = BundleFileInfo::new("payload.bin".to_string(), u64::MAX, 1);

        let error = webfile
            .extract_file_view_by_info(&invalid)
            .expect_err("out-of-bounds entry must be rejected");

        assert!(matches!(error, BinaryError::InvalidData(_)));
    }

    #[test]
    fn duplicate_bundle_entries_charge_sources_and_retained_backings_in_order() {
        let first = minimal_unityfs_bundle("2020.3.0f1");
        let second = minimal_unityfs_bundle(
            "2021.3.0f1-with-a-deliberately-long-revision-for-a-distinct-size",
        );
        assert_ne!(first.len(), second.len());
        let webfile_bytes = webfile_with_entries(&[
            ("duplicate.bundle", first.as_slice()),
            ("duplicate.bundle", second.as_slice()),
        ]);
        let outer_cost = u64::try_from(webfile_bytes.len()).unwrap()
            + retained_signature_bytes()
            + retained_directory_bytes(&["duplicate.bundle", "duplicate.bundle"]);
        let first_len = u64::try_from(first.len()).unwrap();
        let second_len = u64::try_from(second.len()).unwrap();
        let first_backing = arc_vec_allocation_bytes::<u8>(1).unwrap();
        let limit = outer_cost + first_len + first_backing;
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: limit,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let webfile = WebFile::from_bytes_with_budget(webfile_bytes, &mut budget)
            .expect("outer WebFile fits the byte budget");

        let error = webfile
            .parse_bundles_with_budget(&mut budget)
            .expect_err("second duplicate entry exceeds the remaining byte budget");

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: actual_limit,
                requested,
            }) if actual_limit == limit && requested == limit + second_len
        ));
        assert_eq!(budget.usage().bytes, limit);
    }

    #[test]
    fn invalid_directory_record_fails_before_nested_budget_is_charged() {
        let mut webfile = WebFile::from_bytes(webfile_with_entry("bundle", b"not-a-bundle"))
            .expect("parse test WebFile");
        webfile.files[0].offset = u64::MAX;
        let mut budget = AssetLoadBudget::default();

        let error = webfile
            .parse_bundles_with_budget(&mut budget)
            .expect_err("invalid directory range must not be ignored");

        assert!(matches!(error, BinaryError::InvalidData(_)));
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn extracted_file_copy_is_charged_before_allocation() {
        let bytes = webfile_with_entry("payload.bin", b"data");
        let webfile = WebFile::from_bytes(bytes).expect("parse test WebFile");
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 3,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = webfile
            .extract_file_with_budget("payload.bin", &mut budget)
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 3,
                requested: 4,
            })
        ));
        assert_eq!(budget.usage().bytes, 0);
    }
}
