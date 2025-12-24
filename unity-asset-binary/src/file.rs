//! Unified Unity file model (UnityPy-aligned).
//!
//! Unity distributes multiple binary container formats:
//! - AssetBundle containers (UnityFS/UnityWeb/UnityRaw)
//! - SerializedFile assets (`.assets`)
//! - WebFile containers (`UnityWebData*`)
//!
//! This module provides a single entry point to parse them into a tagged enum.

use crate::asset::header::SerializedFileHeader;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use std::path::Path;

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
    let signature = String::from_utf8_lossy(&data[..8]);
    matches!(signature.as_ref(), "UnityFS\0" | "UnityWeb" | "UnityRaw")
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
    if sniff_bundle(&data) {
        let bundle = crate::bundle::load_bundle_from_memory(data)?;
        return Ok(UnityFile::AssetBundle(bundle));
    }

    if sniff_serialized_file(&data) {
        let file = crate::asset::parse_serialized_file(data)?;
        return Ok(UnityFile::SerializedFile(file));
    }

    if let Ok(web) = crate::webfile::WebFile::from_bytes(data) {
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
