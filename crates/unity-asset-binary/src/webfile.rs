//! Unity WebFile parsing
//!
//! WebFiles are Unity's web-optimized format that can contain other files
//! and may be compressed with gzip or brotli.

use crate::bundle::{AssetBundle, BundleFileInfo};
use crate::compression::{decompress_brotli_with_budget, decompress_gzip_with_budget};
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
use std::ops::Range;
use unity_asset_core::AssetLoadBudget;

/// Magic bytes for different compression formats
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
// UnityPy uses the ASCII marker at offset 0x20 as a heuristic.
const BROTLI_MAGIC: &[u8] = b"brotli";

/// Compression type used in WebFile
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFileCompression {
    None,
    Gzip,
    Brotli,
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
        let view = DataView::from_shared_range(data, range)?;
        Self::from_view_with_budget(view, budget)
    }

    fn from_view_with_budget(view: DataView, budget: &mut AssetLoadBudget) -> Result<Self> {
        budget.consume_bytes(u64::try_from(view.len()).map_err(|_| {
            BinaryError::invalid_data("WebFile source length does not fit in u64")
        })?)?;

        // Detect compression with cheap heuristics first (UnityPy-style).
        let mut probe = BinaryReader::new(view.as_bytes(), ByteOrder::Little);
        let probed = Self::detect_compression(&mut probe)?;

        // Decompress if necessary, with a brotli fallback for non-heuristic streams.
        let (compression, decompressed_data, signature) = match probed {
            WebFileCompression::Gzip => {
                let decompressed = DataView::from_shared(SharedBytes::from_vec(
                    decompress_gzip_with_budget(view.as_bytes(), budget)?,
                ));
                let signature = read_webfile_signature(decompressed.as_bytes())?;
                (WebFileCompression::Gzip, decompressed, signature)
            }
            WebFileCompression::Brotli => {
                let decompressed = DataView::from_shared(SharedBytes::from_vec(
                    decompress_brotli_with_budget(view.as_bytes(), budget)?,
                ));
                let signature = read_webfile_signature(decompressed.as_bytes())?;
                (WebFileCompression::Brotli, decompressed, signature)
            }
            WebFileCompression::None => {
                // Attempt uncompressed parse first.
                if let Ok(signature) = read_webfile_signature(view.as_bytes()) {
                    (WebFileCompression::None, view, signature)
                } else {
                    // Some brotli streams (including UnityPy's own WebFile.save output) do not
                    // match the 0x20 marker heuristic. Try brotli decompression as a fallback.
                    let decompressed = DataView::from_shared(SharedBytes::from_vec(
                        decompress_brotli_with_budget(view.as_bytes(), budget)?,
                    ));
                    let signature = read_webfile_signature(decompressed.as_bytes())?;
                    (WebFileCompression::Brotli, decompressed, signature)
                }
            }
        };

        // Create reader for decompressed data
        let mut reader = BinaryReader::new(decompressed_data.as_bytes(), ByteOrder::Little);
        // Consume the signature we already validated.
        let _ = reader.read_cstring()?;

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

        // Read file entries
        let mut files = Vec::new();
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
            budget.consume_entries(1)?;
            files.try_reserve(1).map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve a WebFile directory entry: {error}"
                ))
            })?;
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

fn read_webfile_signature(data: &[u8]) -> Result<String> {
    let mut reader = BinaryReader::new(data, ByteOrder::Little);
    let signature = reader.read_cstring()?;
    if !signature.starts_with("UnityWebData") && !signature.starts_with("TuanjieWebData") {
        return Err(BinaryError::invalid_signature(
            "UnityWebData or TuanjieWebData",
            &signature,
        ));
    }
    Ok(signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{AssetLoadLimits, BudgetError};

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
        assert_eq!(budget.usage().entries, 1);
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
        let requested = u64::try_from(webfile_bytes.len() + bundle_bytes.len())
            .expect("test source sizes fit in u64");
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
        assert_eq!(
            budget.usage().bytes,
            u64::try_from(webfile_bytes.len()).unwrap()
        );
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
    fn duplicate_bundle_entries_charge_their_own_source_lengths_in_order() {
        let first = minimal_unityfs_bundle("2020.3.0f1");
        let second = minimal_unityfs_bundle(
            "2021.3.0f1-with-a-deliberately-long-revision-for-a-distinct-size",
        );
        assert_ne!(first.len(), second.len());
        let webfile_bytes = webfile_with_entries(&[
            ("duplicate.bundle", first.as_slice()),
            ("duplicate.bundle", second.as_slice()),
        ]);
        let outer_len = u64::try_from(webfile_bytes.len()).unwrap();
        let first_len = u64::try_from(first.len()).unwrap();
        let second_len = u64::try_from(second.len()).unwrap();
        let limit = outer_len + first_len;
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
            }) if actual_limit == limit && requested == outer_len + first_len + second_len
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
