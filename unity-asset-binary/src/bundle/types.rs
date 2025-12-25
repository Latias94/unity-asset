//! Bundle data structures
//!
//! This module defines the core data structures used for bundle processing.

use super::header::BundleHeader;
use crate::asset::Asset;
use crate::compression::CompressionBlock;
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
struct LazyDecompress {
    source: DataView,
    block_data_start: usize,
    max_memory: Option<usize>,
}

/// Information about a file within the bundle
///
/// Represents a single file entry in the bundle's directory structure.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BundleFileInfo {
    /// Offset within the bundle data
    pub offset: u64,
    /// Size of the file
    pub size: u64,
    /// File name
    pub name: String,
}

impl BundleFileInfo {
    /// Create a new BundleFileInfo
    pub fn new(name: String, offset: u64, size: u64) -> Self {
        Self { name, offset, size }
    }

    /// Check if this file has valid properties
    pub fn is_valid(&self) -> bool {
        !self.name.is_empty() && self.size > 0
    }

    /// Get the end offset of this file
    pub fn end_offset(&self) -> u64 {
        self.offset.checked_add(self.size).unwrap_or(u64::MAX)
    }
}

/// Directory node in the bundle
///
/// Represents a node in the bundle's internal directory structure,
/// which can be either a file or a directory.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DirectoryNode {
    /// Node name
    pub name: String,
    /// Offset in the bundle
    pub offset: u64,
    /// Size of the data
    pub size: u64,
    /// Flags (indicates file type, compression, etc.)
    pub flags: u32,
}

impl DirectoryNode {
    /// Create a new DirectoryNode
    pub fn new(name: String, offset: u64, size: u64, flags: u32) -> Self {
        Self {
            name,
            offset,
            size,
            flags,
        }
    }

    /// Check if this node represents a file
    pub fn is_file(&self) -> bool {
        // Unity uses bit 2 (0x4) to indicate files, not bit 0 (0x1)
        (self.flags & 0x4) != 0
    }

    /// Check if this node represents a directory
    pub fn is_directory(&self) -> bool {
        !self.is_file()
    }

    /// Check if this node's data is compressed
    pub fn is_compressed(&self) -> bool {
        (self.flags & 0x2) != 0
    }

    /// Get the end offset of this node
    pub fn end_offset(&self) -> u64 {
        self.offset.checked_add(self.size).unwrap_or(u64::MAX)
    }
}

/// A Unity AssetBundle
///
/// This structure represents a complete Unity AssetBundle with all its
/// metadata, compression information, and contained assets.
#[derive(Debug)]
pub struct AssetBundle {
    /// Bundle header
    pub header: BundleHeader,
    /// Compression blocks
    pub blocks: Vec<CompressionBlock>,
    /// Directory nodes
    pub nodes: Vec<DirectoryNode>,
    /// File information
    pub files: Vec<BundleFileInfo>,
    /// Contained assets
    pub assets: Vec<Asset>,
    /// Asset file names within the bundle (aligned with `assets` indices).
    pub asset_names: Vec<String>,
    /// Raw source view for legacy bundles (UnityWeb/UnityRaw). UnityFS uses decompressed blocks data.
    legacy_source: Option<DataView>,
    /// Decompressed bundle data (UnityFS blocks data), initialized lazily.
    decompressed: OnceLock<Arc<[u8]>>,
    decompress_lock: Mutex<()>,
    lazy: Mutex<Option<LazyDecompress>>,
    decompressed_len: u64,
}

impl AssetBundle {
    /// Create a new AssetBundle
    pub fn new(header: BundleHeader, data: Vec<u8>) -> Self {
        let decompressed_len = data.len() as u64;
        let decompressed: Arc<[u8]> = data.into();
        let lock = OnceLock::new();
        let _ = lock.set(decompressed);
        Self {
            header,
            blocks: Vec::new(),
            nodes: Vec::new(),
            files: Vec::new(),
            assets: Vec::new(),
            asset_names: Vec::new(),
            legacy_source: None,
            decompressed: lock,
            decompress_lock: Mutex::new(()),
            lazy: Mutex::new(None),
            decompressed_len,
        }
    }

    pub(crate) fn new_empty(header: BundleHeader) -> Self {
        Self {
            header,
            blocks: Vec::new(),
            nodes: Vec::new(),
            files: Vec::new(),
            assets: Vec::new(),
            asset_names: Vec::new(),
            legacy_source: None,
            decompressed: OnceLock::new(),
            decompress_lock: Mutex::new(()),
            lazy: Mutex::new(None),
            decompressed_len: 0,
        }
    }

    pub(crate) fn set_decompressed_len(&mut self, len: u64) {
        self.decompressed_len = len;
    }

    pub(crate) fn set_legacy_source(&mut self, source: DataView) {
        self.legacy_source = Some(source);
    }

    pub(crate) fn legacy_source(&self) -> Option<&DataView> {
        self.legacy_source.as_ref()
    }

    pub(crate) fn set_lazy_unityfs_source(
        &mut self,
        source: DataView,
        block_data_start: usize,
        max_memory: Option<usize>,
    ) {
        let mut guard = self.lazy.lock().unwrap();
        *guard = Some(LazyDecompress {
            source,
            block_data_start,
            max_memory,
        });
    }

    pub(crate) fn set_decompressed_data(&mut self, data: Vec<u8>) {
        self.decompressed_len = data.len() as u64;
        let arc: Arc<[u8]> = data.into();
        let _ = self.decompressed.set(arc);
        let mut guard = self.lazy.lock().unwrap();
        *guard = None;
    }

    /// Get the decompressed bundle data, decompressing UnityFS blocks on demand.
    pub fn data_checked(&self) -> Result<&[u8]> {
        if let Some(bytes) = self.decompressed.get() {
            return Ok(bytes.as_ref());
        }

        if self.header.is_legacy() {
            return self
                .legacy_source
                .as_ref()
                .map(|v| v.as_bytes())
                .ok_or_else(|| BinaryError::invalid_data("Legacy bundle source is not available"));
        }

        let _guard = self.decompress_lock.lock().unwrap();
        if let Some(bytes) = self.decompressed.get() {
            return Ok(bytes.as_ref());
        }

        let lazy = self.lazy.lock().unwrap().clone().ok_or_else(|| {
            BinaryError::invalid_data(
                "Bundle data is not available (not decompressed and no source)",
            )
        })?;

        let mut reader = BinaryReader::new(lazy.source.as_bytes(), ByteOrder::Big);
        reader.set_position(lazy.block_data_start as u64)?;
        let data = super::compression::BundleCompression::decompress_data_blocks_limited(
            &self.header,
            &self.blocks,
            &mut reader,
            lazy.max_memory,
        )?;
        let arc: Arc<[u8]> = data.into();
        let _ = self.decompressed.set(arc);

        Ok(self
            .decompressed
            .get()
            .ok_or_else(|| BinaryError::generic("Failed to initialize decompressed bundle data"))?
            .as_ref())
    }

    /// Get the raw bundle data if already decompressed, otherwise returns an empty slice.
    pub fn data(&self) -> &[u8] {
        self.decompressed
            .get()
            .map(|v| v.as_ref())
            .or_else(|| self.legacy_source.as_ref().map(|v| v.as_bytes()))
            .unwrap_or(&[])
    }

    /// Get a shared reference to the decompressed bundle data, decompressing on demand.
    pub fn data_arc(&self) -> Result<Arc<[u8]>> {
        let _ = self.data_checked()?;
        self.decompressed
            .get()
            .cloned()
            .ok_or_else(|| BinaryError::generic("Decompressed bundle data missing"))
    }

    /// Get the total size of the bundle
    pub fn size(&self) -> u64 {
        if let Some(bytes) = self.decompressed.get() {
            bytes.len() as u64
        } else if self.header.is_legacy() {
            self.legacy_source
                .as_ref()
                .map(|v| v.len() as u64)
                .unwrap_or(0)
        } else {
            self.decompressed_len
        }
    }

    /// Check if the bundle is compressed
    pub fn is_compressed(&self) -> bool {
        !self.blocks.is_empty()
            && self.blocks.iter().any(|block| {
                block
                    .compression_type()
                    .unwrap_or(crate::compression::CompressionType::None)
                    != crate::compression::CompressionType::None
            })
    }

    /// Get the number of files in the bundle
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Get the number of assets in the bundle
    pub fn asset_count(&self) -> usize {
        self.assets.len()
    }

    /// Find a file by name
    pub fn find_file(&self, name: &str) -> Option<&BundleFileInfo> {
        self.files.iter().find(|file| file.name == name)
    }

    /// Find a node by name
    pub fn find_node(&self, name: &str) -> Option<&DirectoryNode> {
        self.nodes.iter().find(|node| node.name == name)
    }

    /// Get all file names
    pub fn file_names(&self) -> Vec<&str> {
        self.files.iter().map(|file| file.name.as_str()).collect()
    }

    /// Get all node names
    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.iter().map(|node| node.name.as_str()).collect()
    }

    /// Extract data for a specific file
    pub fn extract_file_data(&self, file: &BundleFileInfo) -> crate::error::Result<Vec<u8>> {
        let bytes = self.extract_file_slice(file)?;
        Ok(bytes.to_vec())
    }

    pub fn extract_file_slice(&self, file: &BundleFileInfo) -> crate::error::Result<&[u8]> {
        let end_u64 = file
            .offset
            .checked_add(file.size)
            .ok_or_else(|| crate::error::BinaryError::invalid_data("File offset+size overflow"))?;
        let data = self.data_checked()?;
        if end_u64 > data.len() as u64 {
            return Err(crate::error::BinaryError::invalid_data(
                "File offset/size exceeds bundle data",
            ));
        }

        let start = usize::try_from(file.offset).map_err(|_| {
            crate::error::BinaryError::ResourceLimitExceeded(
                "File offset does not fit in usize".to_string(),
            )
        })?;
        let end = usize::try_from(end_u64).map_err(|_| {
            crate::error::BinaryError::ResourceLimitExceeded(
                "File end offset does not fit in usize".to_string(),
            )
        })?;
        if start > end {
            return Err(crate::error::BinaryError::invalid_data(
                "File slice start exceeds end",
            ));
        }
        Ok(&data[start..end])
    }

    /// Extract data for a specific node
    pub fn extract_node_data(&self, node: &DirectoryNode) -> crate::error::Result<Vec<u8>> {
        let bytes = self.extract_node_slice(node)?;
        Ok(bytes.to_vec())
    }

    pub fn extract_node_slice(&self, node: &DirectoryNode) -> crate::error::Result<&[u8]> {
        let end_u64 = node
            .offset
            .checked_add(node.size)
            .ok_or_else(|| crate::error::BinaryError::invalid_data("Node offset+size overflow"))?;
        let data = self.data_checked()?;
        if end_u64 > data.len() as u64 {
            return Err(crate::error::BinaryError::invalid_data(
                "Node offset/size exceeds bundle data",
            ));
        }

        let start = usize::try_from(node.offset).map_err(|_| {
            crate::error::BinaryError::ResourceLimitExceeded(
                "Node offset does not fit in usize".to_string(),
            )
        })?;
        let end = usize::try_from(end_u64).map_err(|_| {
            crate::error::BinaryError::ResourceLimitExceeded(
                "Node end offset does not fit in usize".to_string(),
            )
        })?;
        if start > end {
            return Err(crate::error::BinaryError::invalid_data(
                "Node slice start exceeds end",
            ));
        }
        Ok(&data[start..end])
    }

    /// Get bundle statistics
    pub fn statistics(&self) -> BundleStatistics {
        let total_compressed_size: u64 = self.blocks.iter().map(|b| b.compressed_size as u64).sum();
        let total_uncompressed_size: u64 =
            self.blocks.iter().map(|b| b.uncompressed_size as u64).sum();

        BundleStatistics {
            total_size: self.size(),
            header_size: self.header.header_size(),
            compressed_size: total_compressed_size,
            uncompressed_size: total_uncompressed_size,
            compression_ratio: if total_uncompressed_size > 0 {
                total_compressed_size as f64 / total_uncompressed_size as f64
            } else {
                1.0
            },
            file_count: self.file_count(),
            asset_count: self.asset_count(),
            block_count: self.blocks.len(),
            node_count: self.nodes.len(),
        }
    }

    /// Validate bundle consistency
    pub fn validate(&self) -> crate::error::Result<()> {
        // Validate header
        self.header.validate()?;

        // Validate files don't exceed bundle size
        for file in &self.files {
            if file.offset.checked_add(file.size).is_none() {
                return Err(crate::error::BinaryError::invalid_data(format!(
                    "File '{}' offset+size overflow",
                    file.name
                )));
            }
            if file.end_offset() > self.size() {
                return Err(crate::error::BinaryError::invalid_data(format!(
                    "File '{}' exceeds bundle size",
                    file.name
                )));
            }
        }

        // Validate nodes don't exceed bundle size
        for node in &self.nodes {
            if node.offset.checked_add(node.size).is_none() {
                return Err(crate::error::BinaryError::invalid_data(format!(
                    "Node '{}' offset+size overflow",
                    node.name
                )));
            }
            if node.end_offset() > self.size() {
                return Err(crate::error::BinaryError::invalid_data(format!(
                    "Node '{}' exceeds bundle size",
                    node.name
                )));
            }
        }

        Ok(())
    }
}

/// Bundle statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleStatistics {
    pub total_size: u64,
    pub header_size: u64,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub compression_ratio: f64,
    pub file_count: usize,
    pub asset_count: usize,
    pub block_count: usize,
    pub node_count: usize,
}

/// Bundle loading options
#[derive(Debug, Clone)]
pub struct BundleLoadOptions {
    /// Whether to load all assets immediately
    pub load_assets: bool,
    /// Whether to decompress all blocks immediately
    pub decompress_blocks: bool,
    /// Whether to validate the bundle structure
    pub validate: bool,
    /// Maximum memory usage for decompression (in bytes)
    pub max_memory: Option<usize>,
    /// Maximum size of compressed blocks info (metadata) in bytes.
    ///
    /// This is a cap on the *compressed* bytes read from the input stream before decompression.
    pub max_compressed_blocks_info_size: Option<usize>,
    /// Maximum size of decompressed blocks info (metadata) in bytes.
    pub max_blocks_info_size: Option<usize>,
    /// Maximum size of the legacy (UnityWeb/UnityRaw) directory *compressed* section in bytes.
    ///
    /// This is a cap on the raw bytes read from the input stream before decompression.
    pub max_legacy_directory_compressed_size: Option<usize>,
    /// Maximum number of compression blocks allowed in metadata.
    pub max_blocks: usize,
    /// Maximum number of directory nodes / file entries allowed in metadata.
    pub max_nodes: usize,
}

impl Default for BundleLoadOptions {
    fn default() -> Self {
        Self {
            load_assets: true,
            decompress_blocks: false, // Lazy decompression by default
            validate: true,
            max_memory: Some(1024 * 1024 * 1024), // 1GB default limit
            max_compressed_blocks_info_size: Some(64 * 1024 * 1024), // 64MB compressed metadata cap
            max_blocks_info_size: Some(64 * 1024 * 1024), // 64MB metadata cap
            max_legacy_directory_compressed_size: Some(64 * 1024 * 1024), // 64MB legacy dir cap
            max_blocks: 1_000_000,
            max_nodes: 1_000_000,
        }
    }
}

impl BundleLoadOptions {
    /// Create options for fast loading (minimal processing)
    pub fn fast() -> Self {
        Self {
            load_assets: false,
            decompress_blocks: false,
            validate: false,
            max_memory: None,
            max_compressed_blocks_info_size: None,
            max_blocks_info_size: None,
            max_legacy_directory_compressed_size: None,
            max_blocks: usize::MAX,
            max_nodes: usize::MAX,
        }
    }

    /// Create options for complete loading (all processing)
    pub fn complete() -> Self {
        Self {
            load_assets: true,
            decompress_blocks: true,
            validate: true,
            max_memory: Some(2048 * 1024 * 1024), // 2GB for complete loading
            max_compressed_blocks_info_size: Some(128 * 1024 * 1024), // 128MB compressed metadata cap
            max_blocks_info_size: Some(128 * 1024 * 1024),            // 128MB metadata cap
            max_legacy_directory_compressed_size: Some(128 * 1024 * 1024), // 128MB legacy dir cap
            max_blocks: 2_000_000,
            max_nodes: 2_000_000,
        }
    }
}
