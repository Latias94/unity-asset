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
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

#[derive(Debug)]
struct UnityFsBlockCache {
    source: DataView,
    block_data_start: usize,
    max_memory: Option<usize>,
    max_block_cache_memory: Option<usize>,
    max_compressed_block_size: Option<usize>,
    compressed_starts: Vec<u64>,
    uncompressed_starts: Vec<u64>,
    cached: Vec<Option<Arc<[u8]>>>,
    cached_bytes: usize,
    cached_blocks: usize,
    tick: u64,
    last_tick: Vec<u64>,
    lru: VecDeque<(usize, u64)>,
}

#[derive(Debug, Clone)]
struct LazyDecompress {
    source: DataView,
    block_data_start: usize,
    max_memory: Option<usize>,
    max_compressed_block_size: Option<usize>,
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
        self.offset.saturating_add(self.size)
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
        self.offset.saturating_add(self.size)
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
    unityfs_cache: Mutex<Option<UnityFsBlockCache>>,
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
            unityfs_cache: Mutex::new(None),
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
            unityfs_cache: Mutex::new(None),
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
        max_block_cache_memory: Option<usize>,
        max_compressed_block_size: Option<usize>,
    ) -> Result<()> {
        if block_data_start > source.len() {
            return Err(BinaryError::invalid_data(format!(
                "UnityFS block data start {} exceeds available bytes {}",
                block_data_start,
                source.len()
            )));
        }
        let available_compressed = (source.len() - block_data_start) as u64;

        let mut guard = self.lazy.lock().unwrap();
        *guard = Some(LazyDecompress {
            source,
            block_data_start,
            max_memory,
            max_compressed_block_size,
        });

        let mut compressed_starts = Vec::with_capacity(self.blocks.len());
        let mut uncompressed_starts = Vec::with_capacity(self.blocks.len());
        let mut comp_cursor: u64 = 0;
        let mut uncomp_cursor: u64 = 0;
        for block in &self.blocks {
            if let Some(limit) = max_compressed_block_size {
                if (block.compressed_size as u64) > (limit as u64) {
                    return Err(BinaryError::ResourceLimitExceeded(format!(
                        "Block compressed size {} exceeds max_compressed_block_size {}",
                        block.compressed_size, limit
                    )));
                }
            }
            compressed_starts.push(comp_cursor);
            uncompressed_starts.push(uncomp_cursor);
            comp_cursor = comp_cursor
                .checked_add(block.compressed_size as u64)
                .ok_or_else(|| BinaryError::invalid_data("Total compressed size overflow"))?;
            uncomp_cursor = uncomp_cursor
                .checked_add(block.uncompressed_size as u64)
                .ok_or_else(|| BinaryError::invalid_data("Total uncompressed size overflow"))?;
        }
        if comp_cursor > available_compressed {
            return Err(BinaryError::invalid_data(format!(
                "Total compressed block bytes {} exceeds available bytes {}",
                comp_cursor, available_compressed
            )));
        }

        let mut cache_guard = self.unityfs_cache.lock().unwrap();
        *cache_guard = Some(UnityFsBlockCache {
            source: guard.as_ref().unwrap().source.clone(),
            block_data_start,
            max_memory,
            max_block_cache_memory,
            max_compressed_block_size,
            compressed_starts,
            uncompressed_starts,
            cached: std::iter::repeat_with(|| None)
                .take(self.blocks.len())
                .collect(),
            cached_bytes: 0,
            cached_blocks: 0,
            tick: 0,
            last_tick: vec![0; self.blocks.len()],
            lru: VecDeque::new(),
        });

        Ok(())
    }

    pub(crate) fn set_decompressed_data(&mut self, data: Vec<u8>) {
        self.decompressed_len = data.len() as u64;
        let arc: Arc<[u8]> = data.into();
        let _ = self.decompressed.set(arc);
        let mut guard = self.lazy.lock().unwrap();
        *guard = None;
        let mut cache_guard = self.unityfs_cache.lock().unwrap();
        *cache_guard = None;
    }

    fn extract_range_unityfs(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(size)
            .ok_or_else(|| BinaryError::invalid_data("Range offset+size overflow"))?;
        if end > self.decompressed_len {
            return Err(BinaryError::invalid_data(
                "Requested range exceeds decompressed bundle data",
            ));
        }
        let len_usize = usize::try_from(size).map_err(|_| {
            BinaryError::ResourceLimitExceeded("Requested range does not fit in usize".to_string())
        })?;

        let mut cache_guard = self.unityfs_cache.lock().unwrap();
        let cache = cache_guard.as_mut().ok_or_else(|| {
            BinaryError::invalid_data("Bundle data is not available (no UnityFS lazy cache)")
        })?;

        if let Some(limit) = cache.max_memory {
            if size > limit as u64 {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Requested range size {} exceeds max_memory {}",
                    size, limit
                )));
            }
        }

        let mut out = vec![0u8; len_usize];

        let mut copied = 0usize;

        for (idx, block) in self.blocks.iter().enumerate() {
            let block_start = cache.uncompressed_starts[idx];
            let block_end = block_start
                .checked_add(block.uncompressed_size as u64)
                .ok_or_else(|| BinaryError::invalid_data("Block uncompressed range overflow"))?;

            if block_end <= offset || block_start >= end {
                continue;
            }

            if cache.cached[idx].is_none() {
                if let Some(limit) = cache.max_memory {
                    if (block.uncompressed_size as usize) > limit {
                        return Err(BinaryError::ResourceLimitExceeded(format!(
                            "Block uncompressed size {} exceeds max_memory {}",
                            block.uncompressed_size, limit
                        )));
                    }
                }
                if let Some(limit) = cache.max_block_cache_memory {
                    if (block.uncompressed_size as usize) > limit {
                        return Err(BinaryError::ResourceLimitExceeded(format!(
                            "Block uncompressed size {} exceeds max_unityfs_block_cache_memory {}",
                            block.uncompressed_size, limit
                        )));
                    }
                }
                if let Some(limit) = cache.max_compressed_block_size {
                    if (block.compressed_size as usize) > limit {
                        return Err(BinaryError::ResourceLimitExceeded(format!(
                            "Block compressed size {} exceeds max_compressed_block_size {}",
                            block.compressed_size, limit
                        )));
                    }
                }

                let mut reader = BinaryReader::new(cache.source.as_bytes(), ByteOrder::Big);
                let comp_start = cache.compressed_starts[idx]
                    .checked_add(cache.block_data_start as u64)
                    .ok_or_else(|| BinaryError::invalid_data("Block compressed start overflow"))?;
                reader.set_position(comp_start)?;
                let compressed = reader.read_bytes(block.compressed_size as usize)?;
                let decompressed = block.decompress(&compressed)?;
                let arc: Arc<[u8]> = decompressed.into();
                let arc_len = arc.len();
                cache.cached[idx] = Some(arc);
                cache.cached_bytes = cache.cached_bytes.checked_add(arc_len).ok_or_else(|| {
                    BinaryError::ResourceLimitExceeded(
                        "UnityFS block cache size overflow".to_string(),
                    )
                })?;
                cache.cached_blocks = cache.cached_blocks.saturating_add(1);
            }

            cache.tick = cache.tick.wrapping_add(1);
            cache.last_tick[idx] = cache.tick;
            cache.lru.push_back((idx, cache.tick));

            if let Some(limit) = cache.max_block_cache_memory {
                while cache.cached_bytes > limit {
                    let Some((evict_idx, evict_tick)) = cache.lru.pop_front() else {
                        break;
                    };
                    if cache.last_tick[evict_idx] != evict_tick {
                        continue;
                    }
                    if let Some(data) = cache.cached[evict_idx].take() {
                        cache.cached_bytes = cache.cached_bytes.saturating_sub(data.len());
                        cache.cached_blocks = cache.cached_blocks.saturating_sub(1);
                    }
                }

                if cache.cached_bytes > limit {
                    return Err(BinaryError::ResourceLimitExceeded(format!(
                        "UnityFS block cache memory {} exceeds max_unityfs_block_cache_memory {}",
                        cache.cached_bytes, limit
                    )));
                }
            }

            let data = cache.cached[idx]
                .as_ref()
                .ok_or_else(|| BinaryError::generic("Failed to materialize block cache"))?;

            let copy_start = std::cmp::max(offset, block_start);
            let copy_end = std::cmp::min(end, block_end);
            let src_start = usize::try_from(copy_start - block_start).map_err(|_| {
                BinaryError::ResourceLimitExceeded(
                    "Block-relative start does not fit in usize".to_string(),
                )
            })?;
            let src_end = usize::try_from(copy_end - block_start).map_err(|_| {
                BinaryError::ResourceLimitExceeded(
                    "Block-relative end does not fit in usize".to_string(),
                )
            })?;
            let dst_start = usize::try_from(copy_start - offset).map_err(|_| {
                BinaryError::ResourceLimitExceeded(
                    "Output-relative start does not fit in usize".to_string(),
                )
            })?;
            let dst_end = dst_start + (src_end - src_start);

            out[dst_start..dst_end].copy_from_slice(&data[src_start..src_end]);
            copied += src_end - src_start;
            if copied == len_usize {
                break;
            }
        }

        if copied != len_usize {
            return Err(BinaryError::invalid_data(
                "Failed to extract full range from UnityFS blocks",
            ));
        }

        Ok(out)
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

        if let Some(limit) = lazy.max_compressed_block_size {
            for block in &self.blocks {
                if (block.compressed_size as u64) > (limit as u64) {
                    return Err(BinaryError::ResourceLimitExceeded(format!(
                        "Block compressed size {} exceeds max_compressed_block_size {}",
                        block.compressed_size, limit
                    )));
                }
            }
        }

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
        let mut cache_guard = self.unityfs_cache.lock().unwrap();
        *cache_guard = None;

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
        if self.decompressed.get().is_some() {
            let bytes = self.extract_file_slice(file)?;
            return Ok(bytes.to_vec());
        }

        if self.header.is_legacy() {
            let bytes = self.extract_file_slice(file)?;
            return Ok(bytes.to_vec());
        }

        self.extract_range_unityfs(file.offset, file.size)
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
        if self.decompressed.get().is_some() {
            let bytes = self.extract_node_slice(node)?;
            return Ok(bytes.to_vec());
        }

        if self.header.is_legacy() {
            let bytes = self.extract_node_slice(node)?;
            return Ok(bytes.to_vec());
        }

        self.extract_range_unityfs(node.offset, node.size)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::CompressionBlock;
    use crate::data_view::DataView;
    use crate::shared_bytes::SharedBytes;

    #[test]
    fn unityfs_extract_node_data_is_lazy_and_supports_cross_block_ranges() {
        let header = BundleHeader {
            signature: "UnityFS".to_string(),
            ..Default::default()
        };

        let mut bundle = AssetBundle::new_empty(header);
        bundle.blocks = vec![
            CompressionBlock::new(5, 5, 0),
            CompressionBlock::new(5, 5, 0),
        ];
        bundle.set_decompressed_len(10);

        let bytes: Vec<u8> = (0u8..10u8).collect();
        let view = DataView::from_shared(SharedBytes::from_vec(bytes));
        bundle
            .set_lazy_unityfs_source(view, 0, None, None, None)
            .unwrap();

        let node = DirectoryNode::new("test.bin".to_string(), 3, 6, 0x4);
        let out = bundle.extract_node_data(&node).unwrap();
        assert_eq!(out, vec![3, 4, 5, 6, 7, 8]);

        // Ensure we did not force full-bundle decompression.
        assert!(bundle.decompressed.get().is_none());
        assert!(bundle.data().is_empty());
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
    /// Maximum memory for caching UnityFS *decompressed blocks* during lazy range extraction.
    ///
    /// This controls peak memory when `AssetBundle::extract_node_data` reads only a few nodes from
    /// a large UnityFS without fully decompressing the entire bundle.
    ///
    /// If `None`, block cache growth is unbounded (not recommended for untrusted inputs).
    pub max_unityfs_block_cache_memory: Option<usize>,
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
    /// Maximum size of a single UnityFS compressed data block (in bytes).
    ///
    /// This is a cap on the raw bytes read for each block before decompression. It helps protect
    /// against malicious headers that declare multi-GB compressed blocks.
    pub max_compressed_block_size: Option<usize>,
    /// Maximum number of compression blocks allowed in metadata.
    pub max_blocks: usize,
    /// Maximum number of directory nodes / file entries allowed in metadata.
    pub max_nodes: usize,
}

impl Default for BundleLoadOptions {
    fn default() -> Self {
        Self {
            load_assets: true,
            // Note: UnityFS must decompress blocks to load embedded assets, so `load_assets=true`
            // implies eager decompression even when `decompress_blocks=false`.
            decompress_blocks: false,
            validate: true,
            max_memory: Some(1024 * 1024 * 1024), // 1GB default limit
            max_unityfs_block_cache_memory: Some(1024 * 1024 * 1024), // 1GB default cap
            max_compressed_blocks_info_size: Some(64 * 1024 * 1024), // 64MB compressed metadata cap
            max_blocks_info_size: Some(64 * 1024 * 1024), // 64MB metadata cap
            max_legacy_directory_compressed_size: Some(64 * 1024 * 1024), // 64MB legacy dir cap
            max_compressed_block_size: Some(1024 * 1024 * 1024), // 1GB per-block compressed cap
            max_blocks: 1_000_000,
            max_nodes: 1_000_000,
        }
    }
}

impl BundleLoadOptions {
    /// Create options for lazy loading (validate metadata, but do not preload assets or decompress blocks).
    pub fn lazy() -> Self {
        Self {
            load_assets: false,
            decompress_blocks: false,
            validate: true,
            ..Default::default()
        }
    }

    /// Create options for fast loading (minimal processing)
    pub fn fast() -> Self {
        Self {
            load_assets: false,
            decompress_blocks: false,
            validate: false,
            max_memory: None,
            max_unityfs_block_cache_memory: None,
            max_compressed_blocks_info_size: None,
            max_blocks_info_size: None,
            max_legacy_directory_compressed_size: None,
            max_compressed_block_size: None,
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
            max_unityfs_block_cache_memory: Some(2048 * 1024 * 1024), // 2GB cap
            max_compressed_blocks_info_size: Some(128 * 1024 * 1024), // 128MB compressed metadata cap
            max_blocks_info_size: Some(128 * 1024 * 1024),            // 128MB metadata cap
            max_legacy_directory_compressed_size: Some(128 * 1024 * 1024), // 128MB legacy dir cap
            max_compressed_block_size: Some(2048 * 1024 * 1024), // 2GB per-block compressed cap
            max_blocks: 2_000_000,
            max_nodes: 2_000_000,
        }
    }
}
