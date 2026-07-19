//! Bundle data structures
//!
//! This module defines the core data structures used for bundle processing.

use super::header::BundleHeader;
use crate::asset::SerializedFile;
use crate::compression::{CompressionBlock, decompressor_scratch_bytes};
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::shared_bytes::SharedBytes;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use unity_asset_core::AssetLoadBudget;

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
    #[cfg(test)]
    peak_cached_bytes: usize,
    lru_previous: Vec<Option<usize>>,
    lru_next: Vec<Option<usize>>,
    lru_head: Option<usize>,
    lru_tail: Option<usize>,
    #[cfg(test)]
    lru_evictions: usize,
}

impl UnityFsBlockCache {
    fn touch(&mut self, index: usize) -> Result<()> {
        if self.cached.get(index).and_then(Option::as_ref).is_none() {
            return Err(BinaryError::generic(
                "UnityFS LRU touch referenced an uncached block",
            ));
        }
        if self.lru_tail == Some(index) {
            return Ok(());
        }

        self.unlink(index);
        self.link_as_most_recent(index);
        Ok(())
    }

    fn insert(&mut self, index: usize, data: Arc<[u8]>) -> Result<()> {
        let slot = self
            .cached
            .get(index)
            .ok_or_else(|| BinaryError::invalid_data("UnityFS cache index exceeds block count"))?;
        if slot.is_some() {
            return Err(BinaryError::generic(
                "UnityFS cache attempted to insert a duplicate block",
            ));
        }

        let new_cached_bytes = self.cached_bytes.checked_add(data.len()).ok_or_else(|| {
            BinaryError::ResourceLimitExceeded("UnityFS block cache size overflow".to_string())
        })?;
        let new_cached_blocks = self.cached_blocks.checked_add(1).ok_or_else(|| {
            BinaryError::ResourceLimitExceeded(
                "UnityFS block cache entry count overflow".to_string(),
            )
        })?;

        self.cached[index] = Some(data);
        self.cached_bytes = new_cached_bytes;
        self.cached_blocks = new_cached_blocks;
        self.link_as_most_recent(index);
        #[cfg(test)]
        {
            self.peak_cached_bytes = self.peak_cached_bytes.max(self.cached_bytes);
        }
        Ok(())
    }

    fn evict_least_recent(&mut self) -> Result<()> {
        let index = self.lru_head.ok_or_else(|| {
            BinaryError::generic("UnityFS block cache cannot evict enough data before decode")
        })?;
        self.unlink(index);
        let data = self.cached[index]
            .take()
            .ok_or_else(|| BinaryError::generic("UnityFS block cache eviction lost its entry"))?;
        self.cached_bytes = self
            .cached_bytes
            .checked_sub(data.len())
            .ok_or_else(|| BinaryError::generic("UnityFS block cache byte accounting underflow"))?;
        self.cached_blocks = self.cached_blocks.checked_sub(1).ok_or_else(|| {
            BinaryError::generic("UnityFS block cache entry accounting underflow")
        })?;
        #[cfg(test)]
        {
            self.lru_evictions += 1;
        }
        Ok(())
    }

    fn evict_to_retained_limit(&mut self, retained_limit: u64) -> Result<()> {
        while u64::try_from(self.cached_bytes).map_err(|_| {
            BinaryError::ResourceLimitExceeded(
                "UnityFS cached byte count does not fit in u64".to_string(),
            )
        })? > retained_limit
        {
            self.evict_least_recent()?;
        }
        Ok(())
    }

    fn prepare_output_allocation(&mut self, output_size: u64) -> Result<()> {
        let Some(max_memory) = self.max_memory else {
            return Ok(());
        };
        let max_memory = u64::try_from(max_memory).map_err(|_| {
            BinaryError::ResourceLimitExceeded("max_memory does not fit in u64".to_string())
        })?;
        let retained_limit = max_memory.checked_sub(output_size).ok_or_else(|| {
            BinaryError::ResourceLimitExceeded(format!(
                "UnityFS lazy extraction output {output_size} exceeds max_memory {max_memory}"
            ))
        })?;
        self.evict_to_retained_limit(retained_limit)
    }

    fn prepare_block_decode(
        &mut self,
        output_size: u64,
        block: &CompressionBlock,
        scratch_bytes: u64,
    ) -> Result<()> {
        let incoming = u64::from(block.uncompressed_size);
        let mut retained_limit = u64::MAX;

        if let Some(cache_limit) = self.max_block_cache_memory {
            let cache_limit = u64::try_from(cache_limit).map_err(|_| {
                BinaryError::ResourceLimitExceeded(
                    "max_unityfs_block_cache_memory does not fit in u64".to_string(),
                )
            })?;
            retained_limit =
                retained_limit.min(cache_limit.checked_sub(incoming).ok_or_else(|| {
                    BinaryError::ResourceLimitExceeded(format!(
                        "Block uncompressed size {} exceeds max_unityfs_block_cache_memory {}",
                        block.uncompressed_size, cache_limit
                    ))
                })?);
        }

        let fixed_peak = output_size
            .checked_add(u64::from(block.compressed_size))
            .and_then(|peak| peak.checked_add(incoming))
            .and_then(|peak| peak.checked_add(scratch_bytes))
            .ok_or_else(|| {
                BinaryError::ResourceLimitExceeded(
                    "UnityFS lazy decompression peak-memory size overflow".to_string(),
                )
            })?;
        if let Some(max_memory) = self.max_memory {
            let max_memory = u64::try_from(max_memory).map_err(|_| {
                BinaryError::ResourceLimitExceeded("max_memory does not fit in u64".to_string())
            })?;
            retained_limit = retained_limit.min(max_memory.checked_sub(fixed_peak).ok_or_else(
                || {
                    BinaryError::ResourceLimitExceeded(format!(
                        "UnityFS lazy decompression peak memory {fixed_peak} exceeds max_memory {max_memory}"
                    ))
                },
            )?);
        }

        self.evict_to_retained_limit(retained_limit)?;

        if let Some(max_memory) = self.max_memory {
            let max_memory = u64::try_from(max_memory).map_err(|_| {
                BinaryError::ResourceLimitExceeded("max_memory does not fit in u64".to_string())
            })?;
            let retained = u64::try_from(self.cached_bytes).map_err(|_| {
                BinaryError::ResourceLimitExceeded(
                    "UnityFS cached byte count does not fit in u64".to_string(),
                )
            })?;
            let peak = fixed_peak.checked_add(retained).ok_or_else(|| {
                BinaryError::ResourceLimitExceeded(
                    "UnityFS lazy decompression peak-memory size overflow".to_string(),
                )
            })?;
            if peak > max_memory {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "UnityFS lazy decompression peak memory {peak} exceeds max_memory {max_memory}"
                )));
            }
        }

        Ok(())
    }

    fn link_as_most_recent(&mut self, index: usize) {
        let previous_tail = self.lru_tail;
        self.lru_previous[index] = previous_tail;
        self.lru_next[index] = None;
        if let Some(previous_tail) = previous_tail {
            self.lru_next[previous_tail] = Some(index);
        } else {
            self.lru_head = Some(index);
        }
        self.lru_tail = Some(index);
    }

    fn unlink(&mut self, index: usize) {
        let previous = self.lru_previous[index];
        let next = self.lru_next[index];
        if let Some(previous) = previous {
            self.lru_next[previous] = next;
        } else {
            self.lru_head = next;
        }
        if let Some(next) = next {
            self.lru_previous[next] = previous;
        } else {
            self.lru_tail = previous;
        }
        self.lru_previous[index] = None;
        self.lru_next[index] = None;
    }
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
    /// Unity file-stream node flags.
    ///
    /// Bit 0 marks directories, bit 1 marks deleted entries, and bit 2 marks serialized files.
    /// A regular resource file therefore commonly has no flags set.
    pub flags: u32,
}

impl DirectoryNode {
    /// The entry is a directory rather than a file.
    pub const DIRECTORY_FLAG: u32 = 0x1;
    /// The entry is a deleted/tombstone node.
    pub const DELETED_FLAG: u32 = 0x2;
    /// The file contains a Unity serialized file.
    pub const SERIALIZED_FILE_FLAG: u32 = 0x4;

    /// Create a new DirectoryNode
    pub fn new(name: String, offset: u64, size: u64, flags: u32) -> Self {
        Self {
            name,
            offset,
            size,
            flags,
        }
    }

    /// Check if this node represents a live file.
    ///
    /// Deleted/tombstone nodes are not exposed as readable files even when they do not carry the
    /// directory flag.
    pub fn is_file(&self) -> bool {
        !self.is_directory() && !self.is_deleted()
    }

    /// Check if this node represents a directory
    pub fn is_directory(&self) -> bool {
        (self.flags & Self::DIRECTORY_FLAG) != 0
    }

    /// Check if this node is marked as deleted.
    pub fn is_deleted(&self) -> bool {
        (self.flags & Self::DELETED_FLAG) != 0
    }

    /// Check if this file contains a Unity serialized file.
    pub fn is_serialized_file(&self) -> bool {
        self.is_file() && (self.flags & Self::SERIALIZED_FILE_FLAG) != 0
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
    pub assets: Vec<SerializedFile>,
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
            if let Some(limit) = max_compressed_block_size
                && (block.compressed_size as u64) > (limit as u64)
            {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "Block compressed size {} exceeds max_compressed_block_size {}",
                    block.compressed_size, limit
                )));
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
            #[cfg(test)]
            peak_cached_bytes: 0,
            lru_previous: vec![None; self.blocks.len()],
            lru_next: vec![None; self.blocks.len()],
            lru_head: None,
            lru_tail: None,
            #[cfg(test)]
            lru_evictions: 0,
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

    fn extract_range_unityfs_with_budget(
        &self,
        offset: u64,
        size: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<u8>> {
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
        budget.consume_bytes(size)?;

        let mut cache_guard = self.unityfs_cache.lock().unwrap();
        let cache = cache_guard.as_mut().ok_or_else(|| {
            BinaryError::invalid_data("Bundle data is not available (no UnityFS lazy cache)")
        })?;

        cache.prepare_output_allocation(size)?;

        let mut out = Vec::new();
        out.try_reserve_exact(len_usize).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {len_usize} extracted bundle bytes: {error}"
            ))
        })?;
        out.resize(len_usize, 0);

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
                if let Some(limit) = cache.max_compressed_block_size
                    && (block.compressed_size as usize) > limit
                {
                    return Err(BinaryError::ResourceLimitExceeded(format!(
                        "Block compressed size {} exceeds max_compressed_block_size {}",
                        block.compressed_size, limit
                    )));
                }

                let block_data_start = u64::try_from(cache.block_data_start).map_err(|_| {
                    BinaryError::invalid_data("UnityFS block data start does not fit in u64")
                })?;
                let comp_start = cache.compressed_starts[idx]
                    .checked_add(block_data_start)
                    .ok_or_else(|| BinaryError::invalid_data("Block compressed start overflow"))?;
                let comp_end = comp_start
                    .checked_add(u64::from(block.compressed_size))
                    .ok_or_else(|| BinaryError::invalid_data("Block compressed end overflow"))?;
                let comp_start_usize = usize::try_from(comp_start).map_err(|_| {
                    BinaryError::invalid_data("Block compressed start does not fit in usize")
                })?;
                let comp_end_usize = usize::try_from(comp_end).map_err(|_| {
                    BinaryError::invalid_data("Block compressed end does not fit in usize")
                })?;
                let uncompressed_size = usize::try_from(block.uncompressed_size).map_err(|_| {
                    BinaryError::invalid_data("Block uncompressed size does not fit in usize")
                })?;
                let scratch_bytes = {
                    let compressed = cache
                        .source
                        .as_bytes()
                        .get(comp_start_usize..comp_end_usize)
                        .ok_or_else(|| {
                            BinaryError::not_enough_data(comp_end_usize, cache.source.len())
                        })?;
                    decompressor_scratch_bytes(
                        compressed,
                        block.compression_type()?,
                        uncompressed_size,
                    )?
                };
                cache.prepare_block_decode(size, block, scratch_bytes)?;
                budget.check_decompression(
                    u64::from(block.compressed_size),
                    u64::from(block.uncompressed_size),
                )?;
                budget.check_compressed_bytes(u64::from(block.compressed_size))?;
                let mut reader = BinaryReader::new(cache.source.as_bytes(), ByteOrder::Big);
                reader.set_position(comp_start)?;
                let compressed_size = usize::try_from(block.compressed_size).map_err(|_| {
                    BinaryError::invalid_data("Block compressed size does not fit in usize")
                })?;
                let compressed = reader.read_bytes(compressed_size)?;
                let decompressed = block.decompress_with_budget(&compressed, budget)?;
                let arc: Arc<[u8]> = decompressed.into();
                cache.insert(idx, arc)?;
            }

            cache.touch(idx)?;

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
        let mut budget = AssetLoadBudget::default();
        self.data_checked_with_budget(&mut budget)
    }

    /// Get the decompressed bundle data through a caller-owned cumulative load budget.
    pub fn data_checked_with_budget(&self, budget: &mut AssetLoadBudget) -> Result<&[u8]> {
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
        let data =
            super::compression::BundleCompression::decompress_data_blocks_limited_with_budget(
                &self.header,
                &self.blocks,
                &mut reader,
                lazy.max_memory,
                budget,
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
        let mut budget = AssetLoadBudget::default();
        self.data_arc_with_budget(&mut budget)
    }

    /// Get shared decompressed data through a caller-owned cumulative load budget.
    pub fn data_arc_with_budget(&self, budget: &mut AssetLoadBudget) -> Result<Arc<[u8]>> {
        if let Some(source) = &self.legacy_source {
            return visible_data_arc_with_budget(source, budget);
        }
        let _ = self.data_checked_with_budget(budget)?;
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
        let mut budget = AssetLoadBudget::default();
        self.extract_file_data_with_budget(file, &mut budget)
    }

    /// Extract file data through a caller-owned cumulative load budget.
    pub fn extract_file_data_with_budget(
        &self,
        file: &BundleFileInfo,
        budget: &mut AssetLoadBudget,
    ) -> crate::error::Result<Vec<u8>> {
        if self.decompressed.get().is_some() {
            let bytes = self.extract_file_slice_with_budget(file, budget)?;
            return copy_bytes_with_budget(bytes, budget);
        }

        if self.header.is_legacy() {
            let bytes = self.extract_file_slice_with_budget(file, budget)?;
            return copy_bytes_with_budget(bytes, budget);
        }

        self.extract_range_unityfs_with_budget(file.offset, file.size, budget)
    }

    pub fn extract_file_slice(&self, file: &BundleFileInfo) -> crate::error::Result<&[u8]> {
        let mut budget = AssetLoadBudget::default();
        self.extract_file_slice_with_budget(file, &mut budget)
    }

    pub fn extract_file_slice_with_budget(
        &self,
        file: &BundleFileInfo,
        budget: &mut AssetLoadBudget,
    ) -> crate::error::Result<&[u8]> {
        let end_u64 = file
            .offset
            .checked_add(file.size)
            .ok_or_else(|| crate::error::BinaryError::invalid_data("File offset+size overflow"))?;
        let data = self.data_checked_with_budget(budget)?;
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
        let mut budget = AssetLoadBudget::default();
        self.extract_node_data_with_budget(node, &mut budget)
    }

    /// Extract node data through a caller-owned cumulative load budget.
    pub fn extract_node_data_with_budget(
        &self,
        node: &DirectoryNode,
        budget: &mut AssetLoadBudget,
    ) -> crate::error::Result<Vec<u8>> {
        if self.decompressed.get().is_some() {
            let bytes = self.extract_node_slice_with_budget(node, budget)?;
            return copy_bytes_with_budget(bytes, budget);
        }

        if self.header.is_legacy() {
            let bytes = self.extract_node_slice_with_budget(node, budget)?;
            return copy_bytes_with_budget(bytes, budget);
        }

        self.extract_range_unityfs_with_budget(node.offset, node.size, budget)
    }

    pub fn extract_node_slice(&self, node: &DirectoryNode) -> crate::error::Result<&[u8]> {
        let mut budget = AssetLoadBudget::default();
        self.extract_node_slice_with_budget(node, &mut budget)
    }

    pub fn extract_node_slice_with_budget(
        &self,
        node: &DirectoryNode,
        budget: &mut AssetLoadBudget,
    ) -> crate::error::Result<&[u8]> {
        let end_u64 = node
            .offset
            .checked_add(node.size)
            .ok_or_else(|| crate::error::BinaryError::invalid_data("Node offset+size overflow"))?;
        let data = self.data_checked_with_budget(budget)?;
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

fn visible_data_arc_with_budget(
    view: &DataView,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<[u8]>> {
    let backing = view.backing_shared();
    if let SharedBytes::Arc(bytes) = backing
        && view.base_offset() == 0
        && view.len() == bytes.len()
    {
        return Ok(bytes);
    }

    let length = u64::try_from(view.len())
        .map_err(|_| BinaryError::invalid_data("Legacy bundle view length does not fit u64"))?;
    budget.consume_bytes(length)?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(view.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {} legacy bundle bytes: {error}",
            view.len()
        ))
    })?;
    copy.extend_from_slice(view.as_bytes());
    Ok(Arc::from(copy))
}

fn copy_bytes_with_budget(bytes: &[u8], budget: &mut AssetLoadBudget) -> Result<Vec<u8>> {
    let len = u64::try_from(bytes.len())
        .map_err(|_| BinaryError::invalid_data("Copied bundle range does not fit in u64"))?;
    budget.consume_bytes(len)?;
    let mut owned = Vec::new();
    owned.try_reserve_exact(bytes.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {} copied bundle bytes: {error}",
            bytes.len()
        ))
    })?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::CompressionBlock;
    use crate::data_view::DataView;
    use crate::shared_bytes::SharedBytes;

    #[test]
    fn directory_node_flags_distinguish_resources_directories_and_tombstones() {
        let resource = DirectoryNode::new("data.resS".to_string(), 0, 0, 0);
        assert!(resource.is_file());
        assert!(!resource.is_directory());
        assert!(!resource.is_serialized_file());
        assert!(!resource.is_deleted());

        let serialized = DirectoryNode::new(
            "data.assets".to_string(),
            0,
            0,
            DirectoryNode::SERIALIZED_FILE_FLAG,
        );
        assert!(serialized.is_file());
        assert!(serialized.is_serialized_file());

        let directory = DirectoryNode::new(
            "folder".to_string(),
            0,
            0,
            DirectoryNode::DIRECTORY_FLAG | 0x10,
        );
        assert!(directory.is_directory());
        assert!(!directory.is_file());

        let deleted = DirectoryNode::new("removed".to_string(), 0, 0, DirectoryNode::DELETED_FLAG);
        assert!(!deleted.is_file());
        assert!(!deleted.is_directory());
        assert!(deleted.is_deleted());

        let deleted_serialized = DirectoryNode::new(
            "removed.assets".to_string(),
            0,
            0,
            DirectoryNode::DELETED_FLAG | DirectoryNode::SERIALIZED_FILE_FLAG,
        );
        assert!(!deleted_serialized.is_file());
        assert!(!deleted_serialized.is_serialized_file());
        assert!(deleted_serialized.is_deleted());
    }

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

    #[test]
    fn unityfs_cache_lru_bookkeeping_is_bounded_by_block_count() {
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
        let source = DataView::from_shared(SharedBytes::from_vec((0u8..10).collect()));
        bundle
            .set_lazy_unityfs_source(source, 0, None, Some(5), None)
            .unwrap();

        let first = DirectoryNode::new("first".to_string(), 0, 1, 0x4);
        for _ in 0..64 {
            assert_eq!(bundle.extract_node_data(&first).unwrap(), vec![0]);
        }

        let cache = bundle.unityfs_cache.lock().unwrap();
        let cache = cache.as_ref().unwrap();
        assert_eq!(cache.cached_bytes, 5);
        assert_eq!(cache.cached_blocks, 1);
        assert_eq!(cache.lru_previous.len(), bundle.blocks.len());
        assert_eq!(cache.lru_next.len(), bundle.blocks.len());
        assert_eq!(cache.lru_head, Some(0));
        assert_eq!(cache.lru_tail, Some(0));
    }

    #[test]
    fn unityfs_cache_evicts_before_decoding_the_next_block() {
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
        let source = DataView::from_shared(SharedBytes::from_vec((0u8..10).collect()));
        bundle
            .set_lazy_unityfs_source(source, 0, Some(11), Some(10), None)
            .unwrap();

        assert_eq!(
            bundle
                .extract_node_data(&DirectoryNode::new("first".to_string(), 0, 1, 0x4))
                .unwrap(),
            vec![0]
        );
        assert_eq!(
            bundle
                .extract_node_data(&DirectoryNode::new("second".to_string(), 5, 1, 0x4))
                .unwrap(),
            vec![5]
        );

        let cache = bundle.unityfs_cache.lock().unwrap();
        let cache = cache.as_ref().unwrap();
        assert_eq!(cache.peak_cached_bytes, 5);
        assert_eq!(cache.cached_bytes, 5);
        assert!(cache.cached[0].is_none());
        assert!(cache.cached[1].is_some());
        assert_eq!(cache.lru_evictions, 1);
    }

    #[test]
    fn unityfs_cache_evicts_in_exact_lru_order() {
        let header = BundleHeader {
            signature: "UnityFS".to_string(),
            ..Default::default()
        };
        let mut bundle = AssetBundle::new_empty(header);
        bundle.blocks = vec![CompressionBlock::new(1, 1, 0); 3];
        bundle.set_decompressed_len(3);
        let source = DataView::from_shared(SharedBytes::from_vec(vec![10, 11, 12]));
        bundle
            .set_lazy_unityfs_source(source, 0, None, Some(2), None)
            .unwrap();

        for offset in [0, 1, 0, 2] {
            let node = DirectoryNode::new(format!("block-{offset}"), offset, 1, 0x4);
            assert_eq!(
                bundle.extract_node_data(&node).unwrap(),
                vec![10 + offset as u8]
            );
        }

        let cache = bundle.unityfs_cache.lock().unwrap();
        let cache = cache.as_ref().unwrap();
        assert!(cache.cached[0].is_some());
        assert!(cache.cached[1].is_none());
        assert!(cache.cached[2].is_some());
        assert_eq!(cache.lru_head, Some(0));
        assert_eq!(cache.lru_tail, Some(2));
        assert_eq!(cache.lru_next[0], Some(2));
        assert_eq!(cache.lru_previous[2], Some(0));
        assert_eq!(cache.lru_evictions, 1);
    }

    #[test]
    fn unityfs_lru_eviction_work_is_linear_in_cache_misses() {
        const BLOCK_COUNT: usize = 2_048;

        let header = BundleHeader {
            signature: "UnityFS".to_string(),
            ..Default::default()
        };
        let mut bundle = AssetBundle::new_empty(header);
        bundle.blocks = vec![CompressionBlock::new(1, 1, 0); BLOCK_COUNT];
        bundle.set_decompressed_len(BLOCK_COUNT as u64);
        let source = DataView::from_shared(SharedBytes::from_vec(vec![0; BLOCK_COUNT]));
        bundle
            .set_lazy_unityfs_source(source, 0, None, Some(1), None)
            .unwrap();

        let node = DirectoryNode::new("all-blocks".to_string(), 0, BLOCK_COUNT as u64, 0x4);
        assert_eq!(bundle.extract_node_data(&node).unwrap().len(), BLOCK_COUNT);

        let cache = bundle.unityfs_cache.lock().unwrap();
        let cache = cache.as_ref().unwrap();
        assert_eq!(cache.lru_evictions, BLOCK_COUNT - 1);
        assert_eq!(cache.cached_blocks, 1);
        assert_eq!(cache.lru_head, Some(BLOCK_COUNT - 1));
        assert_eq!(cache.lru_tail, Some(BLOCK_COUNT - 1));
    }

    #[test]
    fn unityfs_lazy_owned_peak_has_an_exact_max_memory_boundary() {
        fn bundle_with_limit(max_memory: usize) -> AssetBundle {
            let header = BundleHeader {
                signature: "UnityFS".to_string(),
                ..Default::default()
            };
            let mut bundle = AssetBundle::new_empty(header);
            bundle.blocks = vec![CompressionBlock::new(5, 5, 0)];
            bundle.set_decompressed_len(5);
            let source = DataView::from_shared(SharedBytes::from_vec(vec![1, 2, 3, 4, 5]));
            bundle
                .set_lazy_unityfs_source(source, 0, Some(max_memory), Some(5), None)
                .unwrap();
            bundle
        }

        // One output byte + five compressed bytes + five incoming decompressed bytes.
        let exact = bundle_with_limit(11);
        let node = DirectoryNode::new("first-byte".to_string(), 0, 1, 0x4);
        let mut exact_budget = AssetLoadBudget::default();
        assert_eq!(
            exact
                .extract_node_data_with_budget(&node, &mut exact_budget)
                .unwrap(),
            vec![1]
        );
        assert_eq!(exact_budget.usage().bytes, 1);
        assert_eq!(exact_budget.usage().compressed_bytes, 5);
        assert_eq!(exact_budget.usage().decompressed_bytes, 5);

        let short = bundle_with_limit(10);
        let mut short_budget = AssetLoadBudget::default();
        let error = short
            .extract_node_data_with_budget(&node, &mut short_budget)
            .unwrap_err();
        assert!(matches!(
            error,
            BinaryError::ResourceLimitExceeded(message)
                if message.contains("peak memory 11") && message.contains("max_memory 10")
        ));
        assert_eq!(short_budget.usage().bytes, 1);
        assert_eq!(short_budget.usage().compressed_bytes, 0);
        assert_eq!(short_budget.usage().decompressed_bytes, 0);

        let cache = short.unityfs_cache.lock().unwrap();
        let cache = cache.as_ref().unwrap();
        assert_eq!(cache.cached_blocks, 0);
        assert_eq!(cache.peak_cached_bytes, 0);
    }

    #[test]
    fn legacy_data_arc_reuses_full_arc_and_copies_only_a_visible_subrange() {
        let header = BundleHeader {
            signature: "UnityRaw".to_string(),
            ..Default::default()
        };
        let original: Arc<[u8]> = vec![1, 2, 3, 4].into();
        let mut full = AssetBundle::new_empty(header.clone());
        full.set_legacy_source(DataView::from_shared(SharedBytes::from_arc(
            original.clone(),
        )));
        let mut tiny_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: 1,
            ..Default::default()
        })
        .unwrap();
        let reused = full.data_arc_with_budget(&mut tiny_budget).unwrap();
        assert!(Arc::ptr_eq(&original, &reused));
        assert_eq!(tiny_budget.usage().bytes, 0);

        let mut partial = AssetBundle::new_empty(header);
        partial.set_legacy_source(
            DataView::from_shared_range(SharedBytes::from_arc(original.clone()), 1..3).unwrap(),
        );
        let mut exact_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: 2,
            ..Default::default()
        })
        .unwrap();
        let copied = partial.data_arc_with_budget(&mut exact_budget).unwrap();
        assert_eq!(copied.as_ref(), &[2, 3]);
        assert!(!Arc::ptr_eq(&original, &copied));
        assert_eq!(exact_budget.usage().bytes, 2);
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
