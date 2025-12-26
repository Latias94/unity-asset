//! Unified Unity file model (UnityPy-aligned).
//!
//! Unity distributes multiple binary container formats:
//! - AssetBundle containers (UnityFS/UnityWeb/UnityRaw)
//! - SerializedFile assets (`.assets`)
//! - WebFile containers (`UnityWebData*`)
//!
//! This module provides a single entry point to parse them into a tagged enum.

use crate::asset::SerializedFile;
use crate::asset::header::SerializedFileHeader;
use crate::bundle::{AssetBundle, BundleLoadOptions, BundleParser};
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
use std::ops::Range;
use std::path::Path;

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
/// This performs a minimal header parse and validity check.
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
    if data.len() < 20 {
        return false;
    }
    let mut reader = BinaryReader::new(data, ByteOrder::Big);
    let Ok(header) = SerializedFileHeader::from_reader(&mut reader) else {
        return false;
    };
    header.is_valid()
}

/// Parse a Unity binary file from memory, returning a tagged [`UnityFile`] enum.
///
/// Notes:
/// - The detection order is: bundle → serialized file → webfile.
/// - WebFile detection can involve decompression, so it is attempted last.
pub fn load_unity_file_from_memory(data: Vec<u8>) -> Result<UnityFile> {
    let shared = SharedBytes::from_vec(data);
    let len = shared.len();
    load_unity_file_from_shared_range(shared, 0..len)
}

/// Parse a Unity binary file from a shared backing buffer + byte range.
///
/// This is useful for container formats that can provide a view into a larger buffer (e.g. WebFile entries).
pub fn load_unity_file_from_shared_range(
    data: SharedBytes,
    range: Range<usize>,
) -> Result<UnityFile> {
    let view = DataView::from_shared_range(data, range)?;
    let bytes = view.as_bytes();

    if sniff_bundle(bytes) {
        let bundle = crate::bundle::BundleParser::from_shared_range(
            view.backing_shared(),
            view.absolute_range(),
        )?;
        return Ok(UnityFile::AssetBundle(bundle));
    }

    if sniff_serialized_file(bytes) {
        let file = crate::asset::SerializedFileParser::from_shared_range(
            view.backing_shared(),
            view.absolute_range(),
        )?;
        return Ok(UnityFile::SerializedFile(file));
    }

    if let Ok(web) =
        crate::webfile::WebFile::from_shared_range(view.backing_shared(), view.absolute_range())
    {
        return Ok(UnityFile::WebFile(web));
    }

    Err(BinaryError::invalid_format(
        "Unrecognized Unity binary file (not AssetBundle/SerializedFile/WebFile)",
    ))
}

/// Parse a Unity binary file from a filesystem path.
pub fn load_unity_file<P: AsRef<Path>>(path: P) -> Result<UnityFile> {
    #[cfg(feature = "mmap")]
    {
        let file = std::fs::File::open(&path).map_err(|e| {
            BinaryError::generic(format!("Failed to open file {:?}: {}", path.as_ref(), e))
        })?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| {
            BinaryError::generic(format!("Failed to mmap file {:?}: {}", path.as_ref(), e))
        })?;
        let shared = SharedBytes::Mmap(std::sync::Arc::new(mmap));
        let len = shared.len();
        load_unity_file_from_shared_range(shared, 0..len)
    }

    #[cfg(not(feature = "mmap"))]
    {
        let data = std::fs::read(&path).map_err(|e| {
            BinaryError::generic(format!("Failed to read file {:?}: {}", path.as_ref(), e))
        })?;
        load_unity_file_from_memory(data)
    }
}

/// Load an AssetBundle from a filesystem path with explicit parser options.
pub fn load_bundle_file_with_options<P: AsRef<Path>>(
    path: P,
    options: BundleLoadOptions,
) -> Result<AssetBundle> {
    #[cfg(feature = "mmap")]
    {
        let file = std::fs::File::open(&path).map_err(|e| {
            BinaryError::generic(format!("Failed to open file {:?}: {}", path.as_ref(), e))
        })?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| {
            BinaryError::generic(format!("Failed to mmap file {:?}: {}", path.as_ref(), e))
        })?;
        let shared = SharedBytes::Mmap(std::sync::Arc::new(mmap));
        let len = shared.len();
        BundleParser::from_shared_range_with_options(shared, 0..len, options)
    }

    #[cfg(not(feature = "mmap"))]
    {
        let data = std::fs::read(&path).map_err(|e| {
            BinaryError::generic(format!("Failed to read file {:?}: {}", path.as_ref(), e))
        })?;
        BundleParser::from_bytes_with_options(data, options)
    }
}

/// Load a SerializedFile from a filesystem path.
pub fn load_serialized_file<P: AsRef<Path>>(
    path: P,
    preload_object_data: bool,
) -> Result<SerializedFile> {
    #[cfg(feature = "mmap")]
    {
        let file = std::fs::File::open(&path).map_err(|e| {
            BinaryError::generic(format!("Failed to open file {:?}: {}", path.as_ref(), e))
        })?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| {
            BinaryError::generic(format!("Failed to mmap file {:?}: {}", path.as_ref(), e))
        })?;
        let shared = SharedBytes::Mmap(std::sync::Arc::new(mmap));
        let len = shared.len();
        crate::asset::SerializedFileParser::from_shared_range_with_options(
            shared,
            0..len,
            preload_object_data,
        )
    }

    #[cfg(not(feature = "mmap"))]
    {
        let data = std::fs::read(&path).map_err(|e| {
            BinaryError::generic(format!("Failed to read file {:?}: {}", path.as_ref(), e))
        })?;
        crate::asset::SerializedFileParser::from_bytes_with_options(data, preload_object_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_bundle_excludes_uncompressed_webfile() {
        let data = b"UnityWebData1.0\0";
        assert!(!sniff_bundle(data));
    }
}
