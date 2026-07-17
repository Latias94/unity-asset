//! Unified Unity file model (UnityPy-aligned).
//!
//! Unity distributes multiple binary container formats:
//! - AssetBundle containers (UnityFS/UnityWeb/UnityRaw)
//! - SerializedFile assets (`.assets`)
//! - WebFile containers (`UnityWebData*`)
//!
//! This module provides a single entry point to parse them into a tagged enum.

use crate::asset::header::SerializedFileHeader;
use crate::asset::{HeaderLayout, SerializedFile, SerializedFileFormat};
use crate::bundle::{AssetBundle, BundleLoadOptions, BundleParser};
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
#[cfg(not(feature = "mmap"))]
use std::io::Read;
use std::ops::Range;
use std::path::Path;
use unity_asset_core::{AssetLoadBudget, BudgetError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnityFileKind {
    AssetBundle,
    SerializedFile,
    WebFile,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum UnityFile {
    AssetBundle(crate::bundle::AssetBundle),
    SerializedFile(crate::asset::SerializedFile),
    WebFile(crate::webfile::WebFile),
}

impl UnityFile {
    pub fn kind(&self) -> UnityFileKind {
        match self {
            UnityFile::AssetBundle(_) => UnityFileKind::AssetBundle,
            UnityFile::SerializedFile(_) => UnityFileKind::SerializedFile,
            UnityFile::WebFile(_) => UnityFileKind::WebFile,
        }
    }

    pub fn as_bundle(&self) -> Option<&crate::bundle::AssetBundle> {
        match self {
            UnityFile::AssetBundle(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_serialized(&self) -> Option<&crate::asset::SerializedFile> {
        match self {
            UnityFile::SerializedFile(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_web(&self) -> Option<&crate::webfile::WebFile> {
        match self {
            UnityFile::WebFile(v) => Some(v),
            _ => None,
        }
    }
}

fn sniff_bundle(data: &[u8]) -> bool {
    looks_like_bundle_prefix(data)
}

/// Return true if the provided byte prefix looks like an AssetBundle container signature.
///
/// Notes:
/// - This mirrors UnityPy-style sniffing and is intentionally conservative.
/// - `UnityWebData*` / `TuanjieWebData*` are WebFile containers and must not be classified as bundles.
pub fn looks_like_bundle_prefix(prefix: &[u8]) -> bool {
    if prefix.len() < 8 {
        return false;
    }
    if prefix.starts_with(b"UnityFS\0") || prefix.starts_with(b"UnityRaw") {
        return true;
    }
    if prefix.starts_with(b"UnityWeb") {
        if prefix.starts_with(b"UnityWebData") || prefix.starts_with(b"TuanjieWebData") {
            return false;
        }
        return true;
    }
    false
}

/// Return true if the provided byte prefix matches the UnityFS bundle signature.
pub fn looks_like_unityfs_bundle_prefix(prefix: &[u8]) -> bool {
    prefix.starts_with(b"UnityFS\0")
}

/// Return true if the provided byte prefix looks like an uncompressed WebFile container signature.
pub fn looks_like_uncompressed_webfile_prefix(prefix: &[u8]) -> bool {
    prefix.starts_with(b"UnityWebData") || prefix.starts_with(b"TuanjieWebData")
}

/// Return true if the provided byte prefix looks like a SerializedFile.
///
/// This only requires the fixed header. Declared regions are checked against the header's declared
/// file size rather than the length of the scan prefix.
pub fn looks_like_serialized_file_prefix(prefix: &[u8]) -> bool {
    sniff_serialized_file(prefix)
}

/// Classify a file by inspecting an in-memory prefix.
///
/// This is a cheap, conservative helper intended for fast directory scans.
pub fn sniff_unity_file_kind_prefix(prefix: &[u8]) -> Option<UnityFileKind> {
    if looks_like_uncompressed_webfile_prefix(prefix) {
        return Some(UnityFileKind::WebFile);
    }
    if looks_like_bundle_prefix(prefix) {
        return Some(UnityFileKind::AssetBundle);
    }
    if looks_like_serialized_file_prefix(prefix) {
        return Some(UnityFileKind::SerializedFile);
    }
    None
}

fn sniff_serialized_file(data: &[u8]) -> bool {
    if data.len() < 16 {
        return false;
    }
    let mut reader = BinaryReader::new(data, ByteOrder::Big);
    let Ok(metadata_size) = reader.read_u32() else {
        return false;
    };
    let Ok(file_size) = reader.read_u32() else {
        return false;
    };
    let Ok(version) = reader.read_u32() else {
        return false;
    };
    let Ok(data_offset) = reader.read_u32() else {
        return false;
    };
    let Ok(format) = SerializedFileFormat::new(version) else {
        return false;
    };

    let (metadata_size, file_size, data_offset) = match format.header_layout() {
        HeaderLayout::Legacy16 => (metadata_size, u64::from(file_size), u64::from(data_offset)),
        HeaderLayout::Standard20 | HeaderLayout::LargeFiles48 => {
            let mut header_reader = BinaryReader::new(data, ByteOrder::Big);
            let Ok(header) = SerializedFileHeader::from_reader(&mut header_reader) else {
                return false;
            };
            (header.metadata_size, header.file_size, header.data_offset)
        }
    };

    format
        .decode_regions(metadata_size, file_size, data_offset, file_size)
        .is_ok()
}

/// Parse a Unity binary file from memory, returning a tagged [`UnityFile`] enum.
///
/// Notes:
/// - The detection order is: bundle → serialized file → webfile.
/// - WebFile detection can involve decompression, so it is attempted last.
pub fn load_unity_file_from_memory(data: Vec<u8>) -> Result<UnityFile> {
    let mut budget = AssetLoadBudget::default();
    load_unity_file_from_memory_with_budget(data, &mut budget)
}

/// Parse a Unity binary file from memory through a caller-owned cumulative load budget.
pub fn load_unity_file_from_memory_with_budget(
    data: Vec<u8>,
    budget: &mut AssetLoadBudget,
) -> Result<UnityFile> {
    let shared = SharedBytes::from_vec(data);
    let len = shared.len();
    load_unity_file_from_shared_range_with_budget(shared, 0..len, budget)
}

/// Parse a Unity binary file from a shared backing buffer + byte range.
///
/// This is useful for container formats that can provide a view into a larger buffer (e.g. WebFile entries).
pub fn load_unity_file_from_shared_range(
    data: SharedBytes,
    range: Range<usize>,
) -> Result<UnityFile> {
    let mut budget = AssetLoadBudget::default();
    load_unity_file_from_shared_range_with_budget(data, range, &mut budget)
}

/// Parse a shared byte range through a caller-owned cumulative load budget.
pub fn load_unity_file_from_shared_range_with_budget(
    data: SharedBytes,
    range: Range<usize>,
    budget: &mut AssetLoadBudget,
) -> Result<UnityFile> {
    let view = DataView::from_shared_range(data, range)?;
    let bytes = view.as_bytes();

    if sniff_bundle(bytes) {
        let bundle = crate::bundle::BundleParser::from_shared_range_with_budget(
            view.backing_shared(),
            view.absolute_range(),
            budget,
        )?;
        return Ok(UnityFile::AssetBundle(bundle));
    }

    if sniff_serialized_file(bytes) {
        let file = crate::asset::SerializedFileParser::from_shared_range_with_budget(
            view.backing_shared(),
            view.absolute_range(),
            budget,
        )?;
        return Ok(UnityFile::SerializedFile(file));
    }

    match crate::webfile::WebFile::from_shared_range_with_budget(
        view.backing_shared(),
        view.absolute_range(),
        budget,
    ) {
        Ok(web) => return Ok(UnityFile::WebFile(web)),
        Err(error) if error.is_resource_error() => return Err(error),
        Err(_) => {}
    }

    Err(BinaryError::invalid_format(
        "Unrecognized Unity binary file (not AssetBundle/SerializedFile/WebFile)",
    ))
}

/// Parse a Unity binary file from a filesystem path.
pub fn load_unity_file<P: AsRef<Path>>(path: P) -> Result<UnityFile> {
    let mut budget = AssetLoadBudget::default();
    load_unity_file_with_budget(path, &mut budget)
}

/// Parse a Unity binary file from a path through a caller-owned cumulative load budget.
pub fn load_unity_file_with_budget<P: AsRef<Path>>(
    path: P,
    budget: &mut AssetLoadBudget,
) -> Result<UnityFile> {
    #[cfg(feature = "mmap")]
    {
        let shared = mmap_file_with_budget(path.as_ref(), budget)?;
        let len = shared.len();
        load_unity_file_from_shared_range_with_budget(shared, 0..len, budget)
    }

    #[cfg(not(feature = "mmap"))]
    {
        let data = read_file_with_budget(path.as_ref(), budget)?;
        load_unity_file_from_memory_with_budget(data, budget)
    }
}

/// Load an AssetBundle from a filesystem path with explicit parser options.
pub fn load_bundle_file_with_options<P: AsRef<Path>>(
    path: P,
    options: BundleLoadOptions,
) -> Result<AssetBundle> {
    let mut budget = AssetLoadBudget::default();
    load_bundle_file_with_options_and_budget(path, options, &mut budget)
}

/// Load an AssetBundle with explicit parser options and a caller-owned load budget.
pub fn load_bundle_file_with_options_and_budget<P: AsRef<Path>>(
    path: P,
    options: BundleLoadOptions,
    budget: &mut AssetLoadBudget,
) -> Result<AssetBundle> {
    #[cfg(feature = "mmap")]
    {
        let shared = mmap_file_with_budget(path.as_ref(), budget)?;
        let len = shared.len();
        BundleParser::from_shared_range_with_options_and_budget(shared, 0..len, options, budget)
    }

    #[cfg(not(feature = "mmap"))]
    {
        let data = read_file_with_budget(path.as_ref(), budget)?;
        BundleParser::from_bytes_with_options_and_budget(data, options, budget)
    }
}

/// Load a SerializedFile from a filesystem path.
pub fn load_serialized_file<P: AsRef<Path>>(
    path: P,
    preload_object_data: bool,
) -> Result<SerializedFile> {
    let mut budget = AssetLoadBudget::default();
    load_serialized_file_with_budget(path, preload_object_data, &mut budget)
}

/// Load a SerializedFile through a caller-owned cumulative load budget.
pub fn load_serialized_file_with_budget<P: AsRef<Path>>(
    path: P,
    preload_object_data: bool,
    budget: &mut AssetLoadBudget,
) -> Result<SerializedFile> {
    #[cfg(feature = "mmap")]
    {
        let shared = mmap_file_with_budget(path.as_ref(), budget)?;
        let len = shared.len();
        crate::asset::SerializedFileParser::from_shared_range_with_options_and_budget(
            shared,
            0..len,
            preload_object_data,
            budget,
        )
    }

    #[cfg(not(feature = "mmap"))]
    {
        let data = read_file_with_budget(path.as_ref(), budget)?;
        crate::asset::SerializedFileParser::from_bytes_with_options_and_budget(
            data,
            preload_object_data,
            budget,
        )
    }
}

#[cfg(feature = "mmap")]
fn mmap_file_with_budget(path: &Path, budget: &AssetLoadBudget) -> Result<SharedBytes> {
    let file = std::fs::File::open(path)
        .map_err(|error| BinaryError::generic(format!("Failed to open file {path:?}: {error}")))?;
    let declared_len = file
        .metadata()
        .map_err(|error| BinaryError::generic(format!("Failed to inspect file {path:?}: {error}")))?
        .len();
    check_source_fits_byte_budget(declared_len, budget)?;
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|error| BinaryError::generic(format!("Failed to mmap file {path:?}: {error}")))?;
    Ok(SharedBytes::Mmap(std::sync::Arc::new(mmap)))
}

#[cfg(not(feature = "mmap"))]
fn read_file_with_budget(path: &Path, budget: &mut AssetLoadBudget) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| BinaryError::generic(format!("Failed to open file {path:?}: {error}")))?;
    let declared_len = file
        .metadata()
        .map_err(|error| BinaryError::generic(format!("Failed to inspect file {path:?}: {error}")))?
        .len();
    check_source_fits_byte_budget(declared_len, budget)?;
    let capacity = usize::try_from(declared_len).map_err(|_| {
        BinaryError::ResourceLimitExceeded(format!(
            "File {path:?} length {declared_len} does not fit in usize"
        ))
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {capacity} bytes for file {path:?}: {error}"
        ))
    })?;

    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut chunk).map_err(|error| {
            BinaryError::generic(format!("Failed to read file {path:?}: {error}"))
        })?;
        if read == 0 {
            break;
        }
        let new_len = bytes
            .len()
            .checked_add(read)
            .ok_or_else(|| BinaryError::memory_error("File read length overflow"))?;
        check_source_fits_byte_budget(
            u64::try_from(new_len)
                .map_err(|_| BinaryError::invalid_data("File length does not fit in u64"))?,
            budget,
        )?;
        bytes.try_reserve(read).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to grow file buffer by {read} bytes for {path:?}: {error}"
            ))
        })?;
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(bytes)
}

/// Checks whether a complete source can fit without charging bytes that parsers have not read yet.
fn check_source_fits_byte_budget(amount: u64, budget: &AssetLoadBudget) -> Result<()> {
    let usage = budget.usage().bytes;
    let requested = usage
        .checked_add(amount)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let limit = budget.limits().max_bytes;
    if requested > limit {
        return Err(BudgetError::Exceeded {
            resource: "bytes",
            limit,
            requested,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

    fn resize_to_scan_prefix(mut bytes: Vec<u8>) -> Vec<u8> {
        bytes.resize(64, 0);
        bytes
    }

    #[test]
    fn serialized_file_sniff_accepts_self_consistent_short_prefixes() {
        let legacy = resize_to_scan_prefix({
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&128_u32.to_be_bytes());
            bytes.extend_from_slice(&4_096_u32.to_be_bytes());
            bytes.extend_from_slice(&8_u32.to_be_bytes());
            bytes.extend_from_slice(&16_u32.to_be_bytes());
            bytes
        });
        assert!(looks_like_serialized_file_prefix(&legacy));

        let standard = resize_to_scan_prefix({
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&128_u32.to_be_bytes());
            bytes.extend_from_slice(&4_096_u32.to_be_bytes());
            bytes.extend_from_slice(&19_u32.to_be_bytes());
            bytes.extend_from_slice(&256_u32.to_be_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&[0; 3]);
            bytes
        });
        assert!(looks_like_serialized_file_prefix(&standard));

        let large_file_size = i64::from(u32::MAX) + 4_096;
        let large = resize_to_scan_prefix({
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&0_u32.to_be_bytes());
            bytes.extend_from_slice(&0_u32.to_be_bytes());
            bytes.extend_from_slice(&22_u32.to_be_bytes());
            bytes.extend_from_slice(&0_u32.to_be_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&[0; 3]);
            bytes.extend_from_slice(&128_u32.to_be_bytes());
            bytes.extend_from_slice(&large_file_size.to_be_bytes());
            bytes.extend_from_slice(&256_i64.to_be_bytes());
            bytes.extend_from_slice(&0_i64.to_be_bytes());
            bytes
        });
        assert_eq!(large.len(), 64);
        assert!(looks_like_serialized_file_prefix(&large));
    }

    #[test]
    fn serialized_file_sniff_rejects_incoherent_short_prefixes() {
        let invalid = resize_to_scan_prefix({
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&512_u32.to_be_bytes());
            bytes.extend_from_slice(&256_u32.to_be_bytes());
            bytes.extend_from_slice(&19_u32.to_be_bytes());
            bytes.extend_from_slice(&128_u32.to_be_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&[0; 3]);
            bytes
        });
        assert!(!looks_like_serialized_file_prefix(&invalid));
    }

    #[test]
    fn source_size_preflight_never_charges_the_budget() {
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 16,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        budget.consume_bytes(4).unwrap();
        let usage_before = budget.usage();

        check_source_fits_byte_budget(12, &budget).unwrap();
        assert_eq!(budget.usage(), usage_before);

        let error = check_source_fits_byte_budget(13, &budget).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 16,
                requested: 17,
            })
        ));
        assert_eq!(budget.usage(), usage_before);
    }

    #[test]
    fn sniff_bundle_excludes_uncompressed_webfile() {
        let data = b"UnityWebData1.0\0";
        assert!(!sniff_bundle(data));
    }

    #[test]
    fn unified_loader_preserves_budget_errors_from_webfiles() {
        let mut bytes = b"UnityWebData1.0\0".to_vec();
        bytes.extend_from_slice(&20_i32.to_le_bytes());
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(bytes.len() - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = load_unity_file_from_memory_with_budget(bytes, &mut budget).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn mmap_path_loaders_reject_oversized_sources_without_charging_budget() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&[0; 64]).unwrap();
        file.flush().unwrap();

        let limits = AssetLoadLimits {
            max_bytes: 63,
            ..AssetLoadLimits::default()
        };

        let mut unified_budget = AssetLoadBudget::new(limits).unwrap();
        let unified_error =
            load_unity_file_with_budget(file.path(), &mut unified_budget).unwrap_err();
        assert!(matches!(
            unified_error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(unified_budget.usage().bytes, 0);

        let mut bundle_budget = AssetLoadBudget::new(limits).unwrap();
        let bundle_error = load_bundle_file_with_options_and_budget(
            file.path(),
            BundleLoadOptions::default(),
            &mut bundle_budget,
        )
        .unwrap_err();
        assert!(matches!(
            bundle_error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(bundle_budget.usage().bytes, 0);

        let mut serialized_budget = AssetLoadBudget::new(limits).unwrap();
        let serialized_error =
            load_serialized_file_with_budget(file.path(), false, &mut serialized_budget)
                .unwrap_err();
        assert!(matches!(
            serialized_error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(serialized_budget.usage().bytes, 0);
    }
}
