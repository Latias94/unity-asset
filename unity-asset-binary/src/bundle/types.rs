//! Bundle data structures
//!
//! This module defines the core data structures used for bundle processing.

use serde::{Deserialize, Serialize};
use crate::asset::Asset;
use crate::compression::CompressionBlock;
use super::header::BundleHeader;

/// Information about a file within the bundle
/// 
/// Represents a single file entry in the bundle's directory structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleFileInfo {
    /// Offset within the bundle data
    pub offset: u64,
    /// Size of the file
    pub size: u64,
    /// File name
    pub name: String,
}

impl Default for BundleFileInfo {
    fn default() -> Self {
        Self {
            offset: 0,
            size: 0,
            name: String::new(),
        }
    }
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
        self.offset + self.size
    }
}

/// Directory node in the bundle
/// 
/// Represents a node in the bundle's internal directory structure,
/// which can be either a file or a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for DirectoryNode {
    fn default() -> Self {
        Self {
            name: String::new(),
            offset: 0,
            size: 0,
            flags: 0,
        }
    }
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
        self.offset + self.size
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
    /// Raw bundle data
    data: Vec<u8>,
}

impl AssetBundle {
    /// Create a new AssetBundle
    pub fn new(header: BundleHeader, data: Vec<u8>) -> Self {
        Self {
            header,
            blocks: Vec::new(),
            nodes: Vec::new(),
            files: Vec::new(),
            assets: Vec::new(),
            data,
        }
    }

    /// Get the raw bundle data
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get mutable access to the raw bundle data
    pub fn data_mut(&mut self) -> &mut Vec<u8> {
        &mut self.data
    }

    /// Get the total size of the bundle
    pub fn size(&self) -> u64 {
        self.data.len() as u64
    }

    /// Check if the bundle is compressed
    pub fn is_compressed(&self) -> bool {
        !self.blocks.is_empty() &&
        self.blocks.iter().any(|block| {
            block.compression_type().unwrap_or(crate::compression::CompressionType::None)
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
        if file.offset + file.size > self.data.len() as u64 {
            return Err(crate::error::BinaryError::invalid_data(
                "File offset/size exceeds bundle data"
            ));
        }

        let start = file.offset as usize;
        let end = (file.offset + file.size) as usize;
        Ok(self.data[start..end].to_vec())
    }

    /// Extract data for a specific node
    pub fn extract_node_data(&self, node: &DirectoryNode) -> crate::error::Result<Vec<u8>> {
        if node.offset + node.size > self.data.len() as u64 {
            return Err(crate::error::BinaryError::invalid_data(
                "Node offset/size exceeds bundle data"
            ));
        }

        let start = node.offset as usize;
        let end = (node.offset + node.size) as usize;
        Ok(self.data[start..end].to_vec())
    }

    /// Get bundle statistics
    pub fn statistics(&self) -> BundleStatistics {
        let total_compressed_size: u64 = self.blocks.iter().map(|b| b.compressed_size as u64).sum();
        let total_uncompressed_size: u64 = self.blocks.iter().map(|b| b.uncompressed_size as u64).sum();
        
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
            if file.end_offset() > self.size() {
                return Err(crate::error::BinaryError::invalid_data(
                    format!("File '{}' exceeds bundle size", file.name)
                ));
            }
        }

        // Validate nodes don't exceed bundle size
        for node in &self.nodes {
            if node.end_offset() > self.size() {
                return Err(crate::error::BinaryError::invalid_data(
                    format!("Node '{}' exceeds bundle size", node.name)
                ));
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
}

impl Default for BundleLoadOptions {
    fn default() -> Self {
        Self {
            load_assets: true,
            decompress_blocks: false, // Lazy decompression by default
            validate: true,
            max_memory: Some(1024 * 1024 * 1024), // 1GB default limit
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
        }
    }

    /// Create options for complete loading (all processing)
    pub fn complete() -> Self {
        Self {
            load_assets: true,
            decompress_blocks: true,
            validate: true,
            max_memory: Some(2048 * 1024 * 1024), // 2GB for complete loading
        }
    }
}
