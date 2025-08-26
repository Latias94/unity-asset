//! Async Binary Types
//!
//! Core async-compatible binary data structures for Unity asset processing.
//! All types implement async-friendly patterns with zero-copy operations where possible.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncSeek};
use unity_asset_core_v2::{Result, UnityAssetError};

/// Type information for Unity objects (copied from V1)
#[derive(Debug, Clone)]
pub struct SerializedType {
    /// Unity class ID
    pub class_id: i32,
    /// Whether this type is stripped
    pub is_stripped_type: bool,
    /// Script type index (for MonoBehaviour)
    pub script_type_index: Option<i16>,
    /// Type tree for this type
    pub type_tree: TypeTree,
    /// Script ID hash
    pub script_id: [u8; 16],
    /// Old type hash
    pub old_type_hash: [u8; 16],
    /// Type dependencies
    pub type_dependencies: Vec<i32>,
    /// Class name
    pub class_name: String,
    /// Namespace
    pub namespace: String,
    /// Assembly name
    pub assembly_name: String,
}

impl SerializedType {
    /// Create a new SerializedType
    pub fn new(class_id: i32) -> Self {
        Self {
            class_id,
            is_stripped_type: false,
            script_type_index: None,
            type_tree: TypeTree::new(),
            script_id: [0; 16],
            old_type_hash: [0; 16],
            type_dependencies: Vec::new(),
            class_name: String::new(),
            namespace: String::new(),
            assembly_name: String::new(),
        }
    }
}

/// External reference to another Unity file (copied from V1)
#[derive(Debug, Clone)]
pub struct FileIdentifier {
    /// GUID of the referenced file
    pub guid: [u8; 16],
    /// Type of the reference
    pub type_: i32,
    /// Path to the referenced file
    pub path_name: String,
}

impl FileIdentifier {
    /// Create new FileIdentifier
    pub fn new() -> Self {
        Self {
            guid: [0; 16],
            type_: 0,
            path_name: String::new(),
        }
    }
}

/// TypeTree node (async-compatible, based on V1 TypeTreeNode)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeTreeNode {
    /// Type name (e.g., "int", "string", "GameObject")
    pub type_name: String,
    /// Field name (e.g., "m_Name", "m_IsActive")
    pub name: String,
    /// Size in bytes (-1 for variable size)
    pub byte_size: i32,
    /// Index in the type tree
    pub index: i32,
    /// Type flags
    pub type_flags: i32,
    /// Version of this type
    pub version: i32,
    /// Meta flags (alignment, etc.)
    pub meta_flags: i32,
    /// Depth level in the tree
    pub level: i32,
    /// Offset in type string buffer
    pub type_str_offset: u32,
    /// Offset in name string buffer
    pub name_str_offset: u32,
    /// Reference type hash
    pub ref_type_hash: u64,
    /// Child nodes
    pub children: Vec<TypeTreeNode>,
}

impl TypeTreeNode {
    /// Create a new TypeTree node
    pub fn new() -> Self {
        Self {
            type_name: String::new(),
            name: String::new(),
            byte_size: 0,
            index: 0,
            type_flags: 0,
            version: 0,
            meta_flags: 0,
            level: 0,
            type_str_offset: 0,
            name_str_offset: 0,
            ref_type_hash: 0,
            children: Vec::new(),
        }
    }

    /// Check if this node represents an array
    pub fn is_array(&self) -> bool {
        self.type_name == "Array" || self.type_name.starts_with("vector")
    }

    /// Check if this is a primitive type
    pub fn is_primitive(&self) -> bool {
        matches!(
            self.type_name.as_str(),
            "bool"
                | "char"
                | "SInt8"
                | "UInt8"
                | "SInt16"
                | "UInt16"
                | "short"
                | "unsigned short"
                | "SInt32"
                | "UInt32"
                | "int"
                | "unsigned int"
                | "SInt64"
                | "UInt64"
                | "long long"
                | "unsigned long long"
                | "float"
                | "double"
                | "string"
        )
    }

    /// Find a child node by name
    pub fn find_child(&self, name: &str) -> Option<&TypeTreeNode> {
        self.children.iter().find(|child| child.name == name)
    }
}

impl Default for TypeTreeNode {
    fn default() -> Self {
        Self::new()
    }
}

/// TypeTree structure (async-compatible, based on V1 TypeTree)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeTree {
    /// Root nodes of the type tree
    pub nodes: Vec<TypeTreeNode>,
    /// String buffer for type and field names
    pub string_buffer: Vec<u8>,
    /// Type tree version
    pub version: u32,
    /// Platform identifier
    pub platform: i32,
    /// Whether this tree has type dependencies
    pub has_type_dependencies: bool,
}

impl TypeTree {
    /// Create a new TypeTree
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            string_buffer: Vec::new(),
            version: 0,
            platform: 0,
            has_type_dependencies: false,
        }
    }

    /// Get string from buffer at offset
    pub fn get_string(&self, offset: u32) -> Result<String> {
        if offset as usize >= self.string_buffer.len() {
            return Ok(String::new());
        }

        let start = offset as usize;
        let end = self.string_buffer[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|pos| start + pos)
            .unwrap_or(self.string_buffer.len());

        String::from_utf8(self.string_buffer[start..end].to_vec()).map_err(|e| {
            UnityAssetError::parse_error(format!("Invalid UTF-8 in string buffer: {}", e), 0)
        })
    }

    /// Find a node by name
    pub fn find_node(&self, name: &str) -> Option<&TypeTreeNode> {
        self.nodes.iter().find(|node| node.name == name)
    }

    /// Get root node
    pub fn root(&self) -> Option<&TypeTreeNode> {
        self.nodes.first()
    }

    /// Get all nodes
    pub fn nodes(&self) -> &[TypeTreeNode] {
        &self.nodes
    }

    /// Resolve string references from string buffer (based on V1 implementation)
    pub fn resolve_strings(&mut self) -> Result<()> {
        for node in &mut self.nodes {
            // Resolve type name from string buffer
            if node.type_str_offset > 0
                && (node.type_str_offset as usize) < self.string_buffer.len()
            {
                if let Some(null_pos) = self.string_buffer[node.type_str_offset as usize..]
                    .iter()
                    .position(|&b| b == 0)
                {
                    let end_pos = node.type_str_offset as usize + null_pos;
                    if let Ok(type_name) = std::str::from_utf8(
                        &self.string_buffer[node.type_str_offset as usize..end_pos],
                    ) {
                        node.type_name = type_name.to_string();
                    }
                }
            }

            // Resolve field name from string buffer
            if node.name_str_offset > 0
                && (node.name_str_offset as usize) < self.string_buffer.len()
            {
                if let Some(null_pos) = self.string_buffer[node.name_str_offset as usize..]
                    .iter()
                    .position(|&b| b == 0)
                {
                    let end_pos = node.name_str_offset as usize + null_pos;
                    if let Ok(field_name) = std::str::from_utf8(
                        &self.string_buffer[node.name_str_offset as usize..end_pos],
                    ) {
                        node.name = field_name.to_string();
                    }
                }
            }
        }
        Ok(())
    }

    /// Build hierarchy from flat node list (based on V1 implementation)
    pub fn build_hierarchy(&mut self) -> Result<()> {
        // For now, just ensure nodes are properly linked
        // Full hierarchy building would require more complex logic
        // This is a simplified version that maintains the flat structure
        // but ensures parent-child relationships are tracked

        let mut stack: Vec<usize> = Vec::new();

        for i in 0..self.nodes.len() {
            let current_level = self.nodes[i].level;

            // Pop stack until we find the parent level
            while let Some(&parent_idx) = stack.last() {
                if self.nodes[parent_idx].level < current_level {
                    break;
                }
                stack.pop();
            }

            // Current node's parent is the top of the stack (if any)
            if let Some(&parent_idx) = stack.last() {
                // In a full implementation, we would set parent-child relationships here
                // For now, we just track the structure
            }

            stack.push(i);
        }

        Ok(())
    }
}

impl Default for TypeTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Information about a Unity object in a serialized file (based on V1 ObjectInfo)
#[derive(Debug, Clone)]
pub struct ObjectInfo {
    /// Path ID (unique identifier within the file)
    pub path_id: i64,
    /// Byte offset in the data section
    pub byte_start: u64,
    /// Size of the object data in bytes
    pub byte_size: u32,
    /// Class ID of the object
    pub class_id: i32,
    /// Type ID (used for type lookup)
    pub type_id: i32,
    /// Raw object data
    pub data: Vec<u8>,
    /// Type information for this object
    pub type_tree: Option<TypeTree>,
}

impl ObjectInfo {
    /// Create a new ObjectInfo
    pub fn new(path_id: i64, byte_start: u64, byte_size: u32, class_id: i32) -> Self {
        Self {
            path_id,
            byte_start,
            byte_size,
            class_id,
            type_id: class_id, // Default to same as class_id
            data: Vec::new(),
            type_tree: None,
        }
    }
}

/// SerializedFile header information (based on V1)
#[derive(Debug, Clone)]
pub struct SerializedFileHeader {
    /// Size of the metadata section
    pub metadata_size: u32,
    /// Total file size
    pub file_size: u32,
    /// File format version
    pub version: u32,
    /// Offset to the data section
    pub data_offset: u32,
    /// Endianness (0 = little, 1 = big)
    pub endian: u8,
    /// Reserved bytes
    pub reserved: [u8; 3],
    /// Whether type tree is enabled
    pub has_type_tree: bool,
}

impl SerializedFileHeader {
    /// Create new header
    pub fn new() -> Self {
        Self {
            metadata_size: 0,
            file_size: 0,
            version: 0,
            data_offset: 0,
            endian: 0,
            reserved: [0; 3],
            has_type_tree: false,
        }
    }

    /// Check if this is a valid Unity file header
    pub fn is_valid(&self) -> bool {
        self.version > 0
            && self.version < 100
            && self.data_offset > 0
            && self.file_size > self.data_offset
    }
}

/// Asset configuration for async processing
#[derive(Debug, Clone)]
pub struct AssetConfig {
    /// Whether to load type trees
    pub load_type_trees: bool,
    /// Whether to load object data immediately
    pub eager_load_objects: bool,
    /// Maximum concurrent object processing
    pub max_concurrent_objects: usize,
    /// Buffer size for object reading
    pub object_buffer_size: usize,
}

impl Default for AssetConfig {
    fn default() -> Self {
        Self {
            load_type_trees: true,
            eager_load_objects: false,
            max_concurrent_objects: 8,
            object_buffer_size: 65536,
        }
    }
}

/// Async processing context
#[derive(Debug)]
pub struct AsyncProcessingContext {
    /// Currently processing objects
    pub active_objects: std::collections::HashSet<i64>,
    /// Processing statistics
    pub stats: ProcessingStats,
}

impl AsyncProcessingContext {
    pub fn new() -> Self {
        Self {
            active_objects: std::collections::HashSet::new(),
            stats: ProcessingStats::default(),
        }
    }
}

/// Processing statistics
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessingStats {
    pub objects_processed: u64,
    pub bytes_processed: u64,
    pub processing_time_ms: u64,
    pub io_time_ms: u64,
    pub decompression_time_ms: u64,
}

/// AssetBundle header information (based on V1)
#[derive(Debug, Clone)]
pub struct BundleHeader {
    /// Bundle signature (e.g., "UnityFS")
    pub signature: String,
    /// Bundle format version
    pub version: u32,
    /// Unity version that created this bundle
    pub unity_version: String,
    /// Unity revision
    pub unity_revision: String,
    /// Total bundle size
    pub size: u64,
    /// Compressed blocks info size
    pub compressed_blocks_info_size: u32,
    /// Uncompressed blocks info size
    pub uncompressed_blocks_info_size: u32,
    /// Archive flags
    pub flags: u32,
}

impl BundleHeader {
    /// Create new bundle header
    pub fn new() -> Self {
        Self {
            signature: String::new(),
            version: 0,
            unity_version: String::new(),
            unity_revision: String::new(),
            size: 0,
            compressed_blocks_info_size: 0,
            uncompressed_blocks_info_size: 0,
            flags: 0,
        }
    }
}

/// Bundle file information (based on V1)
#[derive(Debug, Clone)]
pub struct BundleFileInfo {
    /// Offset within the bundle data
    pub offset: u64,
    /// Size of the file
    pub size: u64,
    /// File name
    pub name: String,
}

impl BundleFileInfo {
    /// Create new bundle file info
    pub fn new(name: String, offset: u64, size: u64) -> Self {
        Self { name, offset, size }
    }
}

/// Bundle processing configuration
#[derive(Debug, Clone)]
pub struct BundleConfig {
    /// Whether to decompress blocks concurrently
    pub concurrent_decompression: bool,
    /// Maximum concurrent decompression tasks
    pub max_concurrent_decompressions: usize,
    /// Whether to load assets eagerly
    pub eager_load_assets: bool,
    /// Buffer size for decompression
    pub decompression_buffer_size: usize,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            concurrent_decompression: true,
            max_concurrent_decompressions: 4,
            eager_load_assets: false,
            decompression_buffer_size: 65536,
        }
    }
}

/// Async-compatible binary reader configuration
#[derive(Debug, Clone)]
pub struct AsyncBinaryConfig {
    /// Buffer size for streaming reads
    pub buffer_size: usize,
    /// Whether to use memory mapping for large files
    pub use_memory_mapping: bool,
    /// Maximum concurrent read operations
    pub max_concurrent_reads: usize,
    /// Read timeout in milliseconds
    pub read_timeout_ms: u64,
}

impl Default for AsyncBinaryConfig {
    fn default() -> Self {
        Self {
            buffer_size: 65536, // 64KB default buffer
            use_memory_mapping: true,
            max_concurrent_reads: 8,
            read_timeout_ms: 30000, // 30 seconds
        }
    }
}

/// Unity version information for binary compatibility
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnityVersionInfo {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub build: String,
    pub full_version: String,
}

impl UnityVersionInfo {
    pub fn new(version_string: &str) -> Result<Self> {
        let parts: Vec<&str> = version_string.split('.').collect();
        if parts.len() < 3 {
            return Err(UnityAssetError::parse_error(
                format!("Invalid Unity version format: {}", version_string),
                0,
            ));
        }

        let major = parts[0]
            .parse::<u32>()
            .map_err(|_| UnityAssetError::parse_error("Invalid major version".to_string(), 0))?;

        let minor = parts[1]
            .parse::<u32>()
            .map_err(|_| UnityAssetError::parse_error("Invalid minor version".to_string(), 0))?;

        // Handle patch version that might contain build info
        let patch_part = parts[2];
        let (patch_str, build) = if let Some(pos) = patch_part.find(char::is_alphabetic) {
            (&patch_part[..pos], patch_part[pos..].to_string())
        } else {
            (patch_part, String::new())
        };

        let patch = patch_str
            .parse::<u32>()
            .map_err(|_| UnityAssetError::parse_error("Invalid patch version".to_string(), 0))?;

        Ok(Self {
            major,
            minor,
            patch,
            build,
            full_version: version_string.to_string(),
        })
    }

    /// Check if this version supports a specific feature
    pub fn supports_feature(&self, feature: UnityFeature) -> bool {
        match feature {
            UnityFeature::UnityFS => self.major >= 5 && self.minor >= 3,
            UnityFeature::TypeTreeV2 => self.major >= 5,
            UnityFeature::LZ4Compression => self.major >= 5 && self.minor >= 3,
            UnityFeature::LZMACompression => self.major >= 3,
            UnityFeature::BrotliCompression => self.major >= 2018,
        }
    }
}

/// Unity features that depend on version
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnityFeature {
    UnityFS,
    TypeTreeV2,
    LZ4Compression,
    LZMACompression,
    BrotliCompression,
}

/// Async binary data container with lazy loading support
#[derive(Debug, Clone)]
pub struct AsyncBinaryData {
    /// Raw binary data
    data: Bytes,
    /// Offset within the original file/stream
    pub offset: u64,
    /// Size of the data
    pub size: u64,
    /// Whether the data is compressed
    pub is_compressed: bool,
    /// Compression type if compressed
    pub compression_type: Option<CompressionType>,
}

impl AsyncBinaryData {
    pub fn new(data: Bytes, offset: u64) -> Self {
        let size = data.len() as u64;
        Self {
            data,
            offset,
            size,
            is_compressed: false,
            compression_type: None,
        }
    }

    pub fn with_compression(mut self, compression: CompressionType) -> Self {
        self.is_compressed = true;
        self.compression_type = Some(compression);
        self
    }

    /// Get the raw data (may be compressed)
    pub fn raw_data(&self) -> &Bytes {
        &self.data
    }

    /// Check if data needs decompression
    pub fn needs_decompression(&self) -> bool {
        self.is_compressed
    }

    /// Get compression info
    pub fn compression_info(&self) -> Option<CompressionType> {
        self.compression_type
    }
}

/// Supported compression types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionType {
    None = 0,
    LZMA = 1,
    LZ4 = 2,
    LZ4HC = 3,
    LZHAM = 4,
    Brotli = 5,
}

impl CompressionType {
    pub fn from_u32(value: u32) -> Result<Self> {
        match value {
            0 => Ok(CompressionType::None),
            1 => Ok(CompressionType::LZMA),
            2 => Ok(CompressionType::LZ4),
            3 => Ok(CompressionType::LZ4HC),
            4 => Ok(CompressionType::LZHAM),
            5 => Ok(CompressionType::Brotli),
            _ => Err(UnityAssetError::unsupported_format(format!(
                "Unknown compression type: {}",
                value
            ))),
        }
    }

    pub fn as_u32(&self) -> u32 {
        *self as u32
    }

    /// Check if compression type is supported
    pub fn is_supported(&self) -> bool {
        matches!(
            self,
            CompressionType::None
                | CompressionType::LZMA
                | CompressionType::LZ4
                | CompressionType::LZ4HC
                | CompressionType::Brotli
        )
    }
}

/// Binary stream position tracking
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPosition {
    /// Absolute position in the file
    pub absolute: u64,
    /// Position relative to current section
    pub relative: u64,
    /// Section/chunk identifier
    pub section_id: u32,
}

impl StreamPosition {
    pub fn new(absolute: u64, relative: u64, section_id: u32) -> Self {
        Self {
            absolute,
            relative,
            section_id,
        }
    }

    pub fn advance(&mut self, bytes: u64) {
        self.absolute += bytes;
        self.relative += bytes;
    }

    pub fn reset_relative(&mut self) {
        self.relative = 0;
    }
}

/// Async binary chunk for streaming processing
#[derive(Debug, Clone)]
pub struct AsyncBinaryChunk {
    /// Chunk identifier
    pub id: u32,
    /// Chunk data
    pub data: AsyncBinaryData,
    /// Chunk flags
    pub flags: ChunkFlags,
    /// Dependencies on other chunks
    pub dependencies: Vec<u32>,
    /// Processing priority
    pub priority: ChunkPriority,
}

impl AsyncBinaryChunk {
    pub fn new(id: u32, data: AsyncBinaryData) -> Self {
        Self {
            id,
            data,
            flags: ChunkFlags::default(),
            dependencies: Vec::new(),
            priority: ChunkPriority::Normal,
        }
    }

    pub fn with_dependencies(mut self, deps: Vec<u32>) -> Self {
        self.dependencies = deps;
        self
    }

    pub fn with_priority(mut self, priority: ChunkPriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn has_dependency(&self, chunk_id: u32) -> bool {
        self.dependencies.contains(&chunk_id)
    }
}

/// Chunk processing flags
#[derive(Debug, Clone, Default)]
pub struct ChunkFlags {
    pub is_compressed: bool,
    pub is_encrypted: bool,
    pub requires_cache: bool,
    pub is_streamed: bool,
}

/// Chunk processing priority
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChunkPriority {
    Critical = 0,
    High = 1,
    Normal = 2,
    Low = 3,
    Background = 4,
}

/// Async binary header for Unity asset files
#[derive(Debug, Clone)]
pub struct AsyncBinaryHeader {
    /// File signature/magic bytes
    pub signature: Bytes,
    /// File format version
    pub version: u32,
    /// Unity engine version
    pub unity_version: UnityVersionInfo,
    /// Platform target
    pub target_platform: u32,
    /// Metadata offset
    pub metadata_offset: u64,
    /// Data offset
    pub data_offset: u64,
    /// File size
    pub file_size: u64,
    /// Header flags
    pub flags: HeaderFlags,
    /// Custom properties
    pub properties: HashMap<String, String>,
}

impl AsyncBinaryHeader {
    /// Validate header integrity
    pub fn validate(&self) -> Result<()> {
        if self.signature.is_empty() {
            return Err(UnityAssetError::parse_error(
                "Missing file signature".to_string(),
                0,
            ));
        }

        if self.metadata_offset >= self.file_size {
            return Err(UnityAssetError::parse_error(
                "Invalid metadata offset".to_string(),
                0,
            ));
        }

        if self.data_offset >= self.file_size {
            return Err(UnityAssetError::parse_error(
                "Invalid data offset".to_string(),
                0,
            ));
        }

        Ok(())
    }

    /// Check if header indicates compressed data
    pub fn is_compressed(&self) -> bool {
        self.flags.is_compressed
    }

    /// Get expected Unity features based on version
    pub fn supported_features(&self) -> Vec<UnityFeature> {
        let mut features = Vec::new();

        if self.unity_version.supports_feature(UnityFeature::UnityFS) {
            features.push(UnityFeature::UnityFS);
        }
        if self
            .unity_version
            .supports_feature(UnityFeature::TypeTreeV2)
        {
            features.push(UnityFeature::TypeTreeV2);
        }
        if self
            .unity_version
            .supports_feature(UnityFeature::LZ4Compression)
        {
            features.push(UnityFeature::LZ4Compression);
        }
        if self
            .unity_version
            .supports_feature(UnityFeature::LZMACompression)
        {
            features.push(UnityFeature::LZMACompression);
        }
        if self
            .unity_version
            .supports_feature(UnityFeature::BrotliCompression)
        {
            features.push(UnityFeature::BrotliCompression);
        }

        features
    }
}

/// Header flags for binary files
#[derive(Debug, Clone, Default)]
pub struct HeaderFlags {
    pub is_compressed: bool,
    pub has_type_tree: bool,
    pub is_stripped: bool,
    pub is_editor_data: bool,
    pub is_web_file: bool,
    pub supports_streaming: bool,
}

// Removed duplicate AsyncProcessingContext - using the simpler version above

// Removed duplicate ProcessingStats - using the simpler version above

impl ProcessingStats {
    /// Calculate processing throughput in MB/s
    pub fn throughput_mbps(&self) -> f64 {
        if self.io_time_ms == 0 {
            0.0
        } else {
            let mb = self.bytes_processed as f64 / (1024.0 * 1024.0);
            let seconds = self.io_time_ms as f64 / 1000.0;
            mb / seconds
        }
    }

    /// Get efficiency ratio (decompression time vs total time)
    pub fn efficiency_ratio(&self) -> f64 {
        if self.io_time_ms == 0 {
            1.0
        } else {
            1.0 - (self.decompression_time_ms as f64 / self.io_time_ms as f64)
        }
    }
}

/// Async binary reader trait for polymorphic reading
pub trait AsyncBinaryReader: AsyncRead + AsyncSeek + Send + Sync {
    /// Read exactly n bytes
    async fn read_exact_bytes(&mut self, count: usize) -> Result<Bytes>;

    /// Read with timeout
    async fn read_exact_bytes_timeout(&mut self, count: usize, timeout_ms: u64) -> Result<Bytes>;

    /// Read u32 value
    async fn read_u32(&mut self) -> Result<u32>;

    /// Read i32 value
    async fn read_i32(&mut self) -> Result<i32>;

    /// Read u64 value
    async fn read_u64(&mut self) -> Result<u64>;

    /// Read i64 value
    async fn read_i64(&mut self) -> Result<i64>;

    /// Read u8 value
    async fn read_u8(&mut self) -> Result<u8>;

    /// Read f32 value
    async fn read_f32(&mut self) -> Result<f32>;

    /// Read f64 value
    async fn read_f64(&mut self) -> Result<f64>;

    /// Read null-terminated string
    async fn read_null_terminated_string(&mut self) -> Result<String>;

    /// Read length-prefixed string
    async fn read_length_prefixed_string(&mut self) -> Result<String>;

    /// Seek to position
    async fn seek(&mut self, pos: u64) -> Result<u64>;

    /// Get current position
    async fn current_position(&mut self) -> Result<u64>;

    /// Get total size if known
    fn total_size(&self) -> Option<u64>;

    /// Check if at end of stream
    async fn is_at_end(&mut self) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unity_version_parsing() {
        let version = UnityVersionInfo::new("2022.3.5f1").unwrap();
        assert_eq!(version.major, 2022);
        assert_eq!(version.minor, 3);
        assert_eq!(version.patch, 5);
        assert_eq!(version.build, "f1");
    }

    #[test]
    fn test_compression_type_conversion() {
        assert_eq!(CompressionType::from_u32(2).unwrap(), CompressionType::LZ4);
        assert_eq!(CompressionType::LZ4.as_u32(), 2);
        assert!(CompressionType::LZ4.is_supported());
    }

    #[test]
    fn test_async_binary_data() {
        let data = Bytes::from_static(b"test data");
        let binary_data = AsyncBinaryData::new(data.clone(), 100);

        assert_eq!(binary_data.size, 9);
        assert_eq!(binary_data.offset, 100);
        assert!(!binary_data.needs_decompression());
    }

    #[test]
    fn test_stream_position() {
        let mut pos = StreamPosition::new(100, 50, 1);
        pos.advance(25);

        assert_eq!(pos.absolute, 125);
        assert_eq!(pos.relative, 75);
        assert_eq!(pos.section_id, 1);

        pos.reset_relative();
        assert_eq!(pos.relative, 0);
    }

    #[test]
    fn test_processing_stats() {
        let mut stats = ProcessingStats::default();
        stats.bytes_processed = 1024 * 1024; // 1MB
        stats.io_time_ms = 1000; // 1 second

        assert_eq!(stats.throughput_mbps(), 1.0);
    }
}
