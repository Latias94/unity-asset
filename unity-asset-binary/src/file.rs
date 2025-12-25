//! Unified Unity file model (UnityPy-aligned).
//!
//! Unity distributes multiple binary container formats:
//! - AssetBundle containers (UnityFS/UnityWeb/UnityRaw)
//! - SerializedFile assets (`.assets`)
//! - WebFile containers (`UnityWebData*`)
//!
//! This module provides a single entry point to parse them into a tagged enum.

use crate::asset::header::SerializedFileHeader;
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnityFileKind {
    AssetBundle,
    SerializedFile,
    WebFile,
}

#[derive(Debug)]
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
    if data.len() < 8 {
        return false;
    }
    if data.starts_with(b"UnityFS\0") || data.starts_with(b"UnityRaw") {
        return true;
    }
    if data.starts_with(b"UnityWeb") {
        // WebFile containers use a longer, distinct signature (`UnityWebData*`) but share the same
        // 8-byte prefix. Avoid mis-classifying uncompressed WebFiles as legacy UnityWeb bundles.
        if data.starts_with(b"UnityWebData") || data.starts_with(b"TuanjieWebData") {
            return false;
        }
        return true;
    }
    false
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
    let data: Arc<[u8]> = data.into();
    let len = data.len();
    load_unity_file_from_shared_range(data, 0..len)
}

/// Parse a Unity binary file from a shared backing buffer + byte range.
///
/// This is useful for container formats that can provide a view into a larger buffer (e.g. WebFile entries).
pub fn load_unity_file_from_shared_range(
    data: Arc<[u8]>,
    range: Range<usize>,
) -> Result<UnityFile> {
    let view = DataView::from_range(data, range)?;
    let bytes = view.as_bytes();

    if sniff_bundle(bytes) {
        let bundle = crate::bundle::BundleParser::from_slice(bytes)?;
        return Ok(UnityFile::AssetBundle(bundle));
    }

    if sniff_serialized_file(bytes) {
        let file = crate::asset::SerializedFileParser::from_shared_range(
            view.backing_arc(),
            view.absolute_range(),
        )?;
        return Ok(UnityFile::SerializedFile(file));
    }

    if let Ok(web) = crate::webfile::WebFile::from_shared_range(
        view.backing_arc(),
        view.absolute_range(),
    ) {
        return Ok(UnityFile::WebFile(web));
    }

    Err(BinaryError::invalid_format(
        "Unrecognized Unity binary file (not AssetBundle/SerializedFile/WebFile)",
    ))
}

/// Parse a Unity binary file from a filesystem path.
pub fn load_unity_file<P: AsRef<Path>>(path: P) -> Result<UnityFile> {
    let data = std::fs::read(&path).map_err(|e| {
        BinaryError::generic(format!("Failed to read file {:?}: {}", path.as_ref(), e))
    })?;
    load_unity_file_from_memory(data)
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
