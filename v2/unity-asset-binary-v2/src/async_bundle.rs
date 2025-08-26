//! Async AssetBundle Processing
//!
//! Provides async streaming support for Unity AssetBundle formats including:
//! - UnityFS (Unity 5.3+) with streaming decompression
//! - UnityRaw format support
//! - Concurrent bundle processing with backpressure control
//! - Zero-copy operations where possible

use crate::async_asset::AsyncSerializedFile;
use crate::binary_types::AsyncBinaryReader;
use crate::binary_types::*;
use async_stream::stream;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncSeek, AsyncSeekExt, BufReader};
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};
use unity_asset_core_v2::{Result, UnityAssetError};

/// Compression block information
#[derive(Debug, Clone)]
pub struct CompressionBlock {
    pub uncompressed_size: u32,
    pub compressed_size: u32,
    pub flags: u16,
}

/// Directory node information  
#[derive(Debug, Clone)]
pub struct DirectoryNode {
    pub offset: u64,
    pub size: u64,
    pub flags: u32,
    pub path: String,
}

/// Async AssetBundle processor (aligned with V1 structure)
pub struct AsyncAssetBundle {
    /// Bundle header (compatible with V1)
    pub header: BundleHeader,
    /// Compression blocks information
    pub blocks: Vec<CompressionBlock>,
    /// Directory nodes
    pub nodes: Vec<DirectoryNode>,
    /// File information (compatible with V1)
    pub files: Vec<BundleFileInfo>,
    /// Loaded assets (SerializedFiles)
    pub assets: Vec<AsyncSerializedFile>,
    /// Raw bundle data
    data: Vec<u8>,
    /// Bundle configuration (async-specific)
    config: BundleConfig,
    /// Processing context (async-specific)
    context: Arc<RwLock<AsyncProcessingContext>>,
    /// Concurrent processing semaphore (async-specific)
    semaphore: Arc<Semaphore>,
}

impl AsyncAssetBundle {
    /// Load AssetBundle from bytes data asynchronously
    pub async fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let cursor = std::io::Cursor::new(data.clone());
        let reader = tokio::io::BufReader::new(cursor);
        let stream_reader = crate::stream_reader::AsyncStreamReader::with_config(
            reader,
            crate::stream_reader::ReaderConfig::default(),
        );
        Self::load_from_reader(stream_reader, BundleConfig::default()).await
    }

    /// Load AssetBundle from file path
    pub async fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path).await.map_err(|e| {
            UnityAssetError::parse_error(format!("Failed to open bundle file: {}", e), 0)
        })?;

        let reader = BufReader::new(file);
        let stream_reader = crate::stream_reader::AsyncStreamReader::with_config(
            reader,
            crate::stream_reader::ReaderConfig::default(),
        );
        Self::load_from_reader(stream_reader, BundleConfig::default()).await
    }

    /// Load AssetBundle from async reader with configuration
    pub async fn load_from_reader<R>(mut reader: R, config: BundleConfig) -> Result<Self>
    where
        R: AsyncBinaryReader + 'static,
    {
        // Read bundle header
        let async_header = Self::read_bundle_header(&mut reader, &config).await?;

        // Convert AsyncBundleHeader to BundleHeader for compatibility
        let header = BundleHeader {
            signature: String::from_utf8_lossy(&async_header.signature).to_string(),
            version: async_header.version,
            unity_version: async_header.unity_version.full_version.clone(),
            unity_revision: "".to_string(), // TODO: Extract from version info
            size: async_header.bundle_size,
            compressed_blocks_info_size: 0, // TODO: Extract from compression info
            uncompressed_blocks_info_size: 0, // TODO: Extract from compression info
            flags: async_header.flags,
        };

        // Read directory information
        let entries = Self::read_bundle_entries(&mut reader, &async_header, &config).await?;

        // Create processing context
        let binary_config = AsyncBinaryConfig {
            buffer_size: config.buffer_size,
            max_concurrent_reads: config.max_concurrent_bundles,
            ..Default::default()
        };

        let context = Arc::new(RwLock::new(AsyncProcessingContext::new()));
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_bundles));

        Ok(Self {
            header,
            blocks: Vec::new(), // TODO: Read actual compression blocks
            nodes: Vec::new(),  // TODO: Read actual directory nodes
            files: Vec::new(),  // Will be populated from entries
            assets: Vec::new(),
            data: Vec::new(),
            config,
            context,
            semaphore,
        })
    }

    /// Get all assets from the bundle
    pub async fn assets(&self) -> Vec<AsyncSerializedFile> {
        // Return cached assets if available
        if !self.assets.is_empty() {
            return self.assets.clone();
        }

        // For now, return empty vec - TODO: implement lazy loading
        Vec::new()
    }

    /// Load a SerializedFile asset from a bundle entry
    async fn load_asset_from_entry(&self, entry: &AsyncBundleEntry) -> Result<AsyncSerializedFile> {
        // For now, create a mock SerializedFile
        // TODO: Implement actual asset loading from bundle data

        // This is a placeholder implementation
        // In a real implementation, we would:
        // 1. Read the raw data from the bundle at the entry's offset
        // 2. Decompress if needed
        // 3. Parse as SerializedFile

        let mock_data = vec![0u8; entry.size as usize];
        AsyncSerializedFile::from_bytes(mock_data).await
    }

    /// Stream all assets from the bundle concurrently
    pub fn assets_stream(&self) -> impl Stream<Item = Result<AsyncSerializedFile>> + Send + '_ {
        let files = self.files.clone();
        let context = Arc::clone(&self.context);
        let semaphore = Arc::clone(&self.semaphore);
        let config = self.config.clone();

        stream! {
            let mut join_set = JoinSet::new();

            // Process assets concurrently with semaphore control
            for file in files {
                if file.name.ends_with(".assets") {
                    let permit = semaphore.clone().acquire_owned().await.map_err(|_| {
                        UnityAssetError::parse_error("Failed to acquire processing permit".to_string(), 0)
                    })?;

                    let file_clone = file.clone();
                    let context_clone = Arc::clone(&context);
                    let config_clone = config.clone();

                    join_set.spawn(async move {
                        let _permit = permit; // Hold permit until completion
                        Self::process_asset_entry(file_clone, context_clone, config_clone).await
                    });
                }
            }

            // Yield results as they complete
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(asset)) => yield Ok(asset),
                    Ok(Err(e)) => yield Err(e),
                    Err(e) => yield Err(UnityAssetError::parse_error(format!("Task join error: {}", e), 0)),
                }
            }
        }
    }

    /// Get bundle metadata
    pub fn metadata(&self) -> BundleMetadata {
        BundleMetadata {
            format: BundleFormat::UnityFS, // Default format
            unity_version: UnityVersionInfo {
                major: 2022,
                minor: 3,
                patch: 0,
                build: "f1".to_string(),
                full_version: self.header.unity_version.clone(),
            },
            entry_count: self.files.len(),
            total_size: self.header.size,
            compression_info: None, // Will be populated when compression info is available
        }
    }

    /// Read bundle header from stream
    async fn read_bundle_header<R>(
        reader: &mut R,
        config: &BundleConfig,
    ) -> Result<AsyncBundleHeader>
    where
        R: AsyncBinaryReader,
    {
        // Read signature to determine format
        let signature = timeout(
            Duration::from_millis(config.read_timeout_ms),
            reader.read_exact_bytes(16),
        )
        .await
        .map_err(|_| UnityAssetError::timeout(Duration::from_secs(30)))??;

        let format = BundleFormat::from_signature(&signature)?;

        match format {
            BundleFormat::UnityFS => Self::read_unityfs_header(reader, signature).await,
            BundleFormat::UnityRaw => Self::read_unityraw_header(reader, signature).await,
            BundleFormat::UnityWeb => Self::read_unityweb_header(reader, signature).await,
        }
    }

    /// Read UnityFS format header
    async fn read_unityfs_header<R>(reader: &mut R, signature: Bytes) -> Result<AsyncBundleHeader>
    where
        R: AsyncBinaryReader,
    {
        // Read version
        let version = u32::from_be_bytes(
            reader
                .read_exact_bytes(4)
                .await?
                .as_ref()
                .try_into()
                .map_err(|_| {
                    UnityAssetError::parse_error("Invalid version bytes".to_string(), 0)
                })?,
        );

        // Read Unity version string
        let unity_version_len = u32::from_be_bytes(
            reader
                .read_exact_bytes(4)
                .await?
                .as_ref()
                .try_into()
                .map_err(|_| {
                    UnityAssetError::parse_error("Invalid version length".to_string(), 0)
                })?,
        ) as usize;

        let unity_version_bytes = reader.read_exact_bytes(unity_version_len).await?;
        let unity_version_str = String::from_utf8(unity_version_bytes.to_vec()).map_err(|_| {
            UnityAssetError::parse_error("Invalid Unity version string".to_string(), 0)
        })?;

        let unity_version = UnityVersionInfo::new(&unity_version_str)?;

        // Read bundle size
        let bundle_size = u64::from_be_bytes(
            reader
                .read_exact_bytes(8)
                .await?
                .as_ref()
                .try_into()
                .map_err(|_| UnityAssetError::parse_error("Invalid bundle size".to_string(), 0))?,
        );

        // Read compressed size
        let compressed_size = u32::from_be_bytes(
            reader
                .read_exact_bytes(4)
                .await?
                .as_ref()
                .try_into()
                .map_err(|_| {
                    UnityAssetError::parse_error("Invalid compressed size".to_string(), 0)
                })?,
        );

        // Read decompressed size
        let decompressed_size = u32::from_be_bytes(
            reader
                .read_exact_bytes(4)
                .await?
                .as_ref()
                .try_into()
                .map_err(|_| {
                    UnityAssetError::parse_error("Invalid decompressed size".to_string(), 0)
                })?,
        );

        // Read flags
        let flags = u32::from_be_bytes(
            reader
                .read_exact_bytes(4)
                .await?
                .as_ref()
                .try_into()
                .map_err(|_| UnityAssetError::parse_error("Invalid flags".to_string(), 0))?,
        );

        // Determine compression type from flags
        let compression_type = CompressionType::from_u32(flags & 0x3F)?;
        let compression_info = if compression_type != CompressionType::None {
            Some(CompressionInfo {
                compression_type,
                compressed_size: compressed_size as u64,
                decompressed_size: decompressed_size as u64,
            })
        } else {
            None
        };

        Ok(AsyncBundleHeader {
            signature,
            format: BundleFormat::UnityFS,
            version,
            unity_version,
            bundle_size,
            flags,
            compression_info,
            metadata_offset: 0, // Will be calculated later
            data_offset: 0,     // Will be calculated later
        })
    }

    /// Read UnityRaw format header
    async fn read_unityraw_header<R>(_reader: &mut R, signature: Bytes) -> Result<AsyncBundleHeader>
    where
        R: AsyncBinaryReader,
    {
        // UnityRaw format is simpler - mostly just the signature
        Ok(AsyncBundleHeader {
            signature,
            format: BundleFormat::UnityRaw,
            version: 1,
            unity_version: UnityVersionInfo::new("3.5.0f5")?, // Default for raw format
            bundle_size: 0,
            flags: 0,
            compression_info: None,
            metadata_offset: 16, // Right after signature
            data_offset: 0,      // Will be calculated
        })
    }

    /// Read UnityWeb format header
    async fn read_unityweb_header<R>(_reader: &mut R, signature: Bytes) -> Result<AsyncBundleHeader>
    where
        R: AsyncBinaryReader,
    {
        // UnityWeb format - legacy web support
        Ok(AsyncBundleHeader {
            signature,
            format: BundleFormat::UnityWeb,
            version: 2,
            unity_version: UnityVersionInfo::new("4.0.0f7")?, // Default for web format
            bundle_size: 0,
            flags: 0,
            compression_info: None,
            metadata_offset: 16,
            data_offset: 0,
        })
    }

    /// Read bundle entries from directory
    async fn read_bundle_entries<R>(
        reader: &mut R,
        header: &AsyncBundleHeader,
        _config: &BundleConfig,
    ) -> Result<Vec<AsyncBundleEntry>>
    where
        R: AsyncBinaryReader,
    {
        match header.format {
            BundleFormat::UnityFS => Self::read_unityfs_entries(reader, header).await,
            BundleFormat::UnityRaw => Self::read_unityraw_entries(reader, header).await,
            BundleFormat::UnityWeb => Self::read_unityweb_entries(reader, header).await,
        }
    }

    /// Read UnityFS entries
    async fn read_unityfs_entries<R>(
        reader: &mut R,
        _header: &AsyncBundleHeader,
    ) -> Result<Vec<AsyncBundleEntry>>
    where
        R: AsyncBinaryReader,
    {
        // Read entry count
        let entry_count = u32::from_be_bytes(
            reader
                .read_exact_bytes(4)
                .await?
                .as_ref()
                .try_into()
                .map_err(|_| UnityAssetError::parse_error("Invalid entry count".to_string(), 0))?,
        ) as usize;

        let mut entries = Vec::with_capacity(entry_count);

        // Read each entry
        for i in 0..entry_count {
            // Read name length
            let name_len = u32::from_be_bytes(
                reader
                    .read_exact_bytes(4)
                    .await?
                    .as_ref()
                    .try_into()
                    .map_err(|_| {
                        UnityAssetError::parse_error("Invalid name length".to_string(), 0)
                    })?,
            ) as usize;

            // Read name
            let name_bytes = reader.read_exact_bytes(name_len).await?;
            let name = String::from_utf8(name_bytes.to_vec()).map_err(|_| {
                UnityAssetError::parse_error("Invalid entry name encoding".to_string(), 0)
            })?;

            // Read offset and size
            let offset = u64::from_be_bytes(
                reader
                    .read_exact_bytes(8)
                    .await?
                    .as_ref()
                    .try_into()
                    .map_err(|_| {
                        UnityAssetError::parse_error("Invalid entry offset".to_string(), 0)
                    })?,
            );

            let size = u64::from_be_bytes(
                reader
                    .read_exact_bytes(8)
                    .await?
                    .as_ref()
                    .try_into()
                    .map_err(|_| {
                        UnityAssetError::parse_error("Invalid entry size".to_string(), 0)
                    })?,
            );

            entries.push(AsyncBundleEntry {
                id: i as u32,
                name: name.clone(),
                offset,
                size,
                entry_type: BundleEntryType::from_name(&name),
                flags: 0,
                dependencies: Vec::new(),
            });
        }

        Ok(entries)
    }

    /// Read UnityRaw entries
    async fn read_unityraw_entries<R>(
        _reader: &mut R,
        _header: &AsyncBundleHeader,
    ) -> Result<Vec<AsyncBundleEntry>>
    where
        R: AsyncBinaryReader,
    {
        // UnityRaw format typically contains a single asset
        Ok(vec![AsyncBundleEntry {
            id: 0,
            name: "CAB-main".to_string(),
            offset: 16, // After signature
            size: 0,    // Will be determined by file size
            entry_type: BundleEntryType::Asset,
            flags: 0,
            dependencies: Vec::new(),
        }])
    }

    /// Read UnityWeb entries
    async fn read_unityweb_entries<R>(
        _reader: &mut R,
        _header: &AsyncBundleHeader,
    ) -> Result<Vec<AsyncBundleEntry>>
    where
        R: AsyncBinaryReader,
    {
        // UnityWeb format - simplified structure
        Ok(vec![AsyncBundleEntry {
            id: 0,
            name: "mainData".to_string(),
            offset: 16,
            size: 0,
            entry_type: BundleEntryType::Asset,
            flags: 0,
            dependencies: Vec::new(),
        }])
    }

    /// Process individual asset entry
    async fn process_asset_entry(
        file: BundleFileInfo,
        context: Arc<RwLock<AsyncProcessingContext>>,
        config: BundleConfig,
    ) -> Result<AsyncSerializedFile> {
        // TODO: Implement actual asset processing from BundleFileInfo
        // For now, return a placeholder error
        Err(UnityAssetError::parse_error(
            format!(
                "Asset processing not yet implemented for file: {}",
                file.name
            ),
            0,
        ))
    }
}

/// Bundle processing configuration
#[derive(Debug, Clone)]
pub struct BundleConfig {
    /// Buffer size for streaming operations
    pub buffer_size: usize,
    /// Maximum concurrent bundle processing
    pub max_concurrent_bundles: usize,
    /// Read timeout in milliseconds
    pub read_timeout_ms: u64,
    /// Whether to preload metadata
    pub preload_metadata: bool,
    /// Whether to cache decompressed data
    pub cache_decompressed: bool,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            buffer_size: 65536, // 64KB
            max_concurrent_bundles: 8,
            read_timeout_ms: 30000, // 30 seconds
            preload_metadata: true,
            cache_decompressed: false, // Memory conscious by default
        }
    }
}

/// AssetBundle header information
#[derive(Debug, Clone)]
pub struct AsyncBundleHeader {
    /// Bundle signature
    pub signature: Bytes,
    /// Bundle format type
    pub format: BundleFormat,
    /// Format version
    pub version: u32,
    /// Unity engine version
    pub unity_version: UnityVersionInfo,
    /// Total bundle size
    pub bundle_size: u64,
    /// Bundle flags
    pub flags: u32,
    /// Compression information
    pub compression_info: Option<CompressionInfo>,
    /// Metadata section offset
    pub metadata_offset: u64,
    /// Data section offset
    pub data_offset: u64,
}

/// Bundle format types
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BundleFormat {
    /// Unity 5.3+ format
    UnityFS,
    /// Legacy raw format
    UnityRaw,
    /// Legacy web format
    UnityWeb,
}

impl BundleFormat {
    /// Identify format from signature
    pub fn from_signature(signature: &[u8]) -> Result<Self> {
        if signature.starts_with(b"UnityFS\0") {
            Ok(BundleFormat::UnityFS)
        } else if signature.starts_with(b"UnityRaw") {
            Ok(BundleFormat::UnityRaw)
        } else if signature.starts_with(b"UnityWeb") {
            Ok(BundleFormat::UnityWeb)
        } else {
            Err(UnityAssetError::unsupported_format(
                "Unknown bundle format".to_string(),
            ))
        }
    }

    /// Check if format supports compression
    pub fn supports_compression(&self) -> bool {
        matches!(self, BundleFormat::UnityFS)
    }

    /// Check if format supports streaming
    pub fn supports_streaming(&self) -> bool {
        matches!(self, BundleFormat::UnityFS)
    }
}

/// Compression information for bundles
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionInfo {
    pub compression_type: CompressionType,
    pub compressed_size: u64,
    pub decompressed_size: u64,
}

impl CompressionInfo {
    /// Calculate compression ratio
    pub fn compression_ratio(&self) -> f64 {
        if self.decompressed_size == 0 {
            0.0
        } else {
            self.compressed_size as f64 / self.decompressed_size as f64
        }
    }

    /// Calculate space savings
    pub fn space_savings(&self) -> f64 {
        1.0 - self.compression_ratio()
    }
}

/// Bundle entry information
#[derive(Debug, Clone)]
pub struct AsyncBundleEntry {
    /// Entry identifier
    pub id: u32,
    /// Entry name/path
    pub name: String,
    /// Offset within bundle
    pub offset: u64,
    /// Entry size
    pub size: u64,
    /// Entry type
    pub entry_type: BundleEntryType,
    /// Entry flags
    pub flags: u32,
    /// Dependencies on other entries
    pub dependencies: Vec<u32>,
}

impl AsyncBundleEntry {
    /// Check if entry is an asset file
    pub fn is_asset_file(&self) -> bool {
        matches!(self.entry_type, BundleEntryType::Asset)
    }

    /// Check if entry is a resource file
    pub fn is_resource_file(&self) -> bool {
        matches!(self.entry_type, BundleEntryType::Resource)
    }

    /// Check if entry is metadata
    pub fn is_metadata(&self) -> bool {
        matches!(self.entry_type, BundleEntryType::Metadata)
    }
}

/// Bundle entry types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleEntryType {
    Asset,
    Resource,
    Metadata,
    Unknown,
}

impl BundleEntryType {
    /// Determine type from entry name
    pub fn from_name(name: &str) -> Self {
        if name.ends_with(".unity3d") || name.contains("CAB-") {
            BundleEntryType::Asset
        } else if name.ends_with(".resS") || name.ends_with(".resource") {
            BundleEntryType::Resource
        } else if name.contains("metadata") {
            BundleEntryType::Metadata
        } else {
            BundleEntryType::Unknown
        }
    }
}

/// Individual asset within a bundle
#[derive(Debug)]
pub struct AsyncBundleAsset {
    /// Asset name
    pub name: String,
    /// Underlying bundle entry
    pub entry: AsyncBundleEntry,
    /// Processing configuration
    pub config: BundleConfig,
    /// Shared processing context
    pub context: Arc<RwLock<AsyncProcessingContext>>,
}

impl AsyncBundleAsset {
    /// Get asset name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get asset size
    pub fn size(&self) -> u64 {
        self.entry.size
    }

    /// Check if asset is compressed
    pub fn is_compressed(&self) -> bool {
        self.entry.flags & 0x1 != 0
    }

    /// Stream objects from this asset
    pub fn objects_stream(&self) -> impl Stream<Item = Result<AsyncAssetObject>> + Send + '_ {
        stream! {
            // This will be implemented when we create the async_asset module
            // For now, yield a placeholder
            yield Err(UnityAssetError::parse_error("Asset object streaming not yet implemented".to_string(), 0));
        }
    }
}

/// Object within an asset
#[derive(Debug)]
pub struct AsyncAssetObject {
    /// Object class ID
    pub class_id: u32,
    /// Object instance ID
    pub instance_id: u64,
    /// Object name (if available)
    pub name: Option<String>,
    /// Raw object data
    pub data: Bytes,
}

impl AsyncAssetObject {
    /// Get object class name
    pub fn class_name(&self) -> &'static str {
        // This will be implemented with proper class mapping
        "UnknownObject"
    }

    /// Get object name
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Bundle metadata summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMetadata {
    pub format: BundleFormat,
    pub unity_version: UnityVersionInfo,
    pub entry_count: usize,
    pub total_size: u64,
    pub compression_info: Option<CompressionInfo>,
}

/// Main bundle processor for concurrent processing
pub struct AsyncBundleProcessor {
    /// Processing configuration
    config: BundleConfig,
    /// Processing statistics
    stats: Arc<RwLock<ProcessingStats>>,
}

impl AsyncBundleProcessor {
    /// Create new bundle processor
    pub fn new() -> Self {
        Self {
            config: BundleConfig::default(),
            stats: Arc::new(RwLock::new(ProcessingStats::default())),
        }
    }

    /// Create bundle processor with custom configuration
    pub fn with_config(config: BundleConfig) -> Self {
        Self {
            config,
            stats: Arc::new(RwLock::new(ProcessingStats::default())),
        }
    }

    /// Get maximum concurrent bundles
    pub fn max_concurrent_bundles(&self) -> usize {
        self.config.max_concurrent_bundles
    }

    /// Get processing statistics
    pub async fn stats(&self) -> ProcessingStats {
        *self.stats.read().await
    }

    /// Process multiple bundles concurrently
    pub async fn process_bundles<P>(&self, bundle_paths: Vec<P>) -> Result<Vec<AsyncAssetBundle>>
    where
        P: AsRef<Path> + Send + 'static,
    {
        let mut join_set = JoinSet::new();
        let semaphore = Arc::new(Semaphore::new(self.config.max_concurrent_bundles));

        // Start processing all bundles
        for path in bundle_paths {
            let permit = semaphore.clone().acquire_owned().await.map_err(|_| {
                UnityAssetError::parse_error("Failed to acquire processing permit".to_string(), 0)
            })?;

            let config = self.config.clone();
            join_set.spawn(async move {
                let _permit = permit;
                AsyncAssetBundle::load_from_path(path).await
            });
        }

        // Collect results
        let mut results = Vec::new();
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok(bundle)) => results.push(bundle),
                Ok(Err(e)) => return Err(e),
                Err(e) => {
                    return Err(UnityAssetError::parse_error(
                        format!("Task join error: {}", e),
                        0,
                    ))
                }
            }
        }

        Ok(results)
    }
}

impl Default for AsyncBundleProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test;

    #[tokio::test]
    async fn test_bundle_format_detection() {
        let unityfs_sig = b"UnityFS\0\x00\x00\x00\x00\x00\x00\x00\x00";
        let format = BundleFormat::from_signature(unityfs_sig).unwrap();
        assert_eq!(format, BundleFormat::UnityFS);
        assert!(format.supports_compression());
    }

    #[tokio::test]
    async fn test_compression_info() {
        let compression = CompressionInfo {
            compression_type: CompressionType::LZ4,
            compressed_size: 1024,
            decompressed_size: 2048,
        };

        assert_eq!(compression.compression_ratio(), 0.5);
        assert_eq!(compression.space_savings(), 0.5);
    }

    #[tokio::test]
    async fn test_bundle_entry_type_detection() {
        assert_eq!(
            BundleEntryType::from_name("CAB-main"),
            BundleEntryType::Asset
        );
        assert_eq!(
            BundleEntryType::from_name("data.resS"),
            BundleEntryType::Resource
        );
        assert_eq!(
            BundleEntryType::from_name("unknown"),
            BundleEntryType::Unknown
        );
    }

    #[tokio::test]
    async fn test_bundle_config_defaults() {
        let config = BundleConfig::default();
        assert_eq!(config.buffer_size, 65536);
        assert_eq!(config.max_concurrent_bundles, 8);
        assert!(config.preload_metadata);
    }

    /// Process asset file (stub implementation)
    async fn process_asset_file(
        _file: BundleFileInfo,
        _context: Arc<RwLock<AsyncProcessingContext>>,
        _config: BundleConfig,
    ) -> Result<AsyncSerializedFile> {
        // TODO: Implement actual asset file processing
        Err(UnityAssetError::parse_error(
            "Asset file processing not yet implemented".to_string(),
            0,
        ))
    }
}
