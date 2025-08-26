//! Async Asset Processing
//!
//! Provides async support for Unity SerializedFile format with streaming object extraction.
//! Supports TypeTree parsing, object deserialization, and concurrent processing.

use crate::binary_types::*;

// Type aliases for compatibility
pub type ExternalReference = FileIdentifier;
pub type AsyncTypeTree = TypeTree;
use crate::stream_reader::{AsyncStreamReader, ReaderConfig};
use async_stream::stream;
use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::BufReader;
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;
use unity_asset_core_v2::{AsyncUnityClass, ObjectMetadata, Result, UnityAssetError, UnityValue};

/// SerializedFile processor
#[derive(Debug, Clone)]
pub struct SerializedFile {
    /// File header information
    pub header: SerializedFileHeader,
    /// Unity version string
    pub unity_version: String,
    /// Target platform
    pub target_platform: i32,
    /// Whether type tree is enabled
    pub enable_type_tree: bool,
    /// Type information
    pub types: Vec<SerializedType>,
    /// Whether big IDs are enabled
    pub big_id_enabled: bool,
    /// Object information entries
    pub objects: Vec<ObjectInfo>,
    /// Script types
    pub script_types: Vec<SerializedType>,
    /// External file references (renamed from externals for V1 compatibility)
    pub externals: Vec<FileIdentifier>,
    /// Reference types
    pub ref_types: Vec<SerializedType>,
    /// User information
    pub user_information: String,
    /// TypeTree definitions (async-specific)
    type_tree: Option<TypeTree>,
    /// Asset configuration
    config: AssetConfig,
    /// Processing context
    context: Arc<RwLock<AsyncProcessingContext>>,
    /// Raw file data
    data: Vec<u8>,
}

impl SerializedFile {
    /// Create AsyncSerializedFile from bytes data asynchronously
    pub async fn from_bytes(data: Vec<u8>) -> Result<Self> {
        // Create async cursor and reader
        let cursor = std::io::Cursor::new(data.clone());
        let reader = tokio::io::BufReader::new(cursor);
        let stream_reader = AsyncStreamReader::with_config(reader, ReaderConfig::default());
        Self::load_from_reader(stream_reader, AssetConfig::default()).await
    }

    /// Load SerializedFile from path
    pub async fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path).await.map_err(|e| {
            UnityAssetError::parse_error(format!("Failed to open asset file: {}", e), 0)
        })?;

        let reader = BufReader::new(file);
        let stream_reader = AsyncStreamReader::with_config(reader, ReaderConfig::default());
        Self::load_from_reader(stream_reader, AssetConfig::default()).await
    }

    /// Load SerializedFile from async reader
    pub async fn load_from_reader<R>(mut reader: R, config: AssetConfig) -> Result<Self>
    where
        R: AsyncBinaryReader + 'static,
    {
        // Read file header
        let header = Self::read_serialized_file_header(&mut reader).await?;

        // Read Unity version and other metadata (based on V1 structure)
        let unity_version = Self::read_unity_version(&mut reader, &header).await?;
        let target_platform = Self::read_target_platform(&mut reader, &header).await?;
        let enable_type_tree = header.has_type_tree;

        // Read type information
        let types = Self::read_types(&mut reader, &header).await?;
        let big_id_enabled = header.version >= 14;

        // Read object information
        let objects = Self::read_object_info(&mut reader, &header).await?;

        // Read script types
        let script_types = Self::read_script_types(&mut reader, &header).await?;

        // Read external references
        let externals = Self::read_externals(&mut reader, &header).await?;

        // Read reference types
        let ref_types = Self::read_ref_types(&mut reader, &header).await?;

        // Read user information
        let user_information = Self::read_user_information(&mut reader, &header).await?;

        // Read TypeTree if present (async-specific)
        let type_tree = if header.has_type_tree {
            Some(Self::read_type_tree(&mut reader, &header).await?)
        } else {
            None
        };

        // Create processing context
        let context = Arc::new(RwLock::new(AsyncProcessingContext::new()));

        Ok(Self {
            header,
            unity_version,
            target_platform,
            enable_type_tree,
            types,
            big_id_enabled,
            objects,
            script_types,
            externals,
            ref_types,
            user_information,
            type_tree,
            config,
            context,
            data: Vec::new(), // Will be populated later
        })
    }

    /// Get all objects from the asset (collect from stream)
    pub async fn get_objects(&self) -> Result<Vec<AsyncUnityClass>> {
        use futures::StreamExt;
        let mut objects = Vec::new();
        let mut stream = Box::pin(self.objects_stream());

        while let Some(result) = stream.next().await {
            objects.push(result?);
        }

        Ok(objects)
    }

    /// Get asset name
    pub fn name(&self) -> &str {
        "AsyncSerializedFile" // Default name, could be enhanced
    }

    /// Stream all objects from the asset
    pub fn objects_stream(&self) -> impl Stream<Item = Result<AsyncUnityClass>> + Send + '_ {
        let objects = self.objects.clone();
        let type_tree = self.type_tree.clone();
        let context = Arc::clone(&self.context);
        let config = self.config.clone();
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_objects));

        stream! {
            let mut join_set = JoinSet::new();

            // Process objects concurrently
            for object_info in objects {
                let permit = semaphore.clone().acquire_owned().await.map_err(|_| {
                    UnityAssetError::parse_error("Failed to acquire object processing permit".to_string(), 0)
                })?;

                let type_tree_clone = type_tree.clone();
                let context_clone = Arc::clone(&context);
                let config_clone = config.clone();
                let object_clone = object_info.clone();

                join_set.spawn(async move {
                    let _permit = permit;
                    Self::process_object(object_clone, type_tree_clone, context_clone, config_clone).await
                });
            }

            // Yield results as they complete
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(unity_class)) => yield Ok(unity_class),
                    Ok(Err(e)) => yield Err(e),
                    Err(e) => yield Err(UnityAssetError::parse_error(format!("Object processing join error: {}", e), 0)),
                }
            }
        }
    }

    /// Get asset metadata
    pub fn metadata(&self) -> AssetMetadata {
        AssetMetadata {
            unity_version: self.header.unity_version.clone(),
            target_platform: self.header.target_platform,
            object_count: self.objects.len(),
            has_type_tree: self.header.has_type_tree,
            external_count: self.externals.len(),
        }
    }

    /// Read SerializedFile header
    async fn read_serialized_file_header<R>(reader: &mut R) -> Result<SerializedFileHeader>
    where
        R: AsyncBinaryReader,
    {
        // Read metadata size
        let metadata_size = reader.read_u32().await?;

        // Read file size
        let file_size = reader.read_u32().await?;

        // Read format version
        let version = reader.read_u32().await?;

        // Read data offset
        let data_offset = reader.read_u32().await?;

        // Check for endianness (Unity can be little or big endian)
        let endian_check = reader.read_u8().await?;
        let is_big_endian = endian_check != 0;

        // Read reserved bytes (usually 3 bytes)
        let _reserved = reader.read_exact_bytes(3).await?;

        // Read Unity version string
        let unity_version_str = reader.read_null_terminated_string().await?;
        let unity_version = UnityVersionInfo::new(&unity_version_str)?;

        // Read target platform
        let target_platform = reader.read_u32().await?;

        // Determine if this file has TypeTree
        let has_type_tree = version >= 7; // Unity 3.0+ typically has TypeTree

        Ok(SerializedFileHeader {
            metadata_size,
            file_size,
            version,
            data_offset,
            is_big_endian,
            unity_version,
            target_platform,
            has_type_tree,
        })
    }

    /// Read TypeTree information
    async fn read_type_tree<R>(
        reader: &mut R,
        header: &SerializedFileHeader,
    ) -> Result<AsyncTypeTree>
    where
        R: AsyncBinaryReader,
    {
        // Read type count
        let type_count = reader.read_u32().await? as usize;
        let mut types = HashMap::with_capacity(type_count);

        for _ in 0..type_count {
            // Read class ID
            let class_id = reader.read_u32().await?;

            // Read type tree nodes for this class
            let nodes = Self::read_type_tree_nodes(reader, header).await?;

            types.insert(
                class_id,
                TypeTreeClass {
                    class_id,
                    nodes,
                    script_type_index: None,
                },
            );
        }

        Ok(AsyncTypeTree {
            nodes: Vec::new(), // TODO: Convert HashMap to Vec<TypeTreeNode>
            string_buffer: Vec::new(),
            version: 0,
            platform: 0,
            has_type_dependencies: false,
        })
    }

    /// Read TypeTree nodes for a specific class
    async fn read_type_tree_nodes<R>(
        reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<Vec<TypeTreeNode>>
    where
        R: AsyncBinaryReader,
    {
        // Read node count
        let node_count = reader.read_u32().await? as usize;
        let mut nodes = Vec::with_capacity(node_count);

        for _ in 0..node_count {
            // Read type name
            let type_name = reader.read_null_terminated_string().await?;

            // Read field name
            let field_name = reader.read_null_terminated_string().await?;

            // Read size
            let size = reader.read_u32().await?;

            // Read index
            let index = reader.read_u32().await?;

            // Read flags
            let flags = reader.read_u32().await?;

            // Read version
            let version = reader.read_u32().await?;

            // Read meta flags
            let meta_flags = reader.read_u32().await?;

            nodes.push(TypeTreeNode {
                type_name,
                field_name,
                size,
                index,
                flags,
                version,
                meta_flags,
                children: Vec::new(), // Children will be resolved in post-processing
            });
        }

        // Post-process to build tree structure
        Self::build_type_tree_hierarchy(nodes)
    }

    /// Build hierarchical structure for TypeTree nodes
    fn build_type_tree_hierarchy(nodes: Vec<TypeTreeNode>) -> Result<Vec<TypeTreeNode>> {
        // This is a simplified version - real implementation would parse the tree structure
        // based on indices and build proper parent-child relationships
        Ok(nodes)
    }

    /// Read object information entries
    async fn read_object_info<R>(
        reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<Vec<ObjectInfo>>
    where
        R: AsyncBinaryReader,
    {
        // Read object count
        let object_count = reader.read_u32().await? as usize;
        let mut objects = Vec::with_capacity(object_count);

        for _ in 0..object_count {
            // Read path ID
            let path_id = reader.read_u64().await?;

            // Read byte offset
            let byte_offset = reader.read_u32().await? as u64;

            // Read byte size
            let byte_size = reader.read_u32().await? as u64;

            // Read class ID
            let class_id = reader.read_u32().await?;

            objects.push(ObjectInfo {
                path_id,
                byte_offset,
                byte_size,
                class_id,
            });
        }

        Ok(objects)
    }

    /// Read external references
    async fn read_externals<R>(
        reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<Vec<ExternalReference>>
    where
        R: AsyncBinaryReader,
    {
        // Read external count
        let external_count = reader.read_u32().await? as usize;
        let mut externals = Vec::with_capacity(external_count);

        for _ in 0..external_count {
            // Read GUID
            let guid_bytes = reader.read_exact_bytes(16).await?;
            let guid = format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                guid_bytes[3], guid_bytes[2], guid_bytes[1], guid_bytes[0],
                guid_bytes[5], guid_bytes[4], guid_bytes[7], guid_bytes[6],
                guid_bytes[8], guid_bytes[9], guid_bytes[10], guid_bytes[11],
                guid_bytes[12], guid_bytes[13], guid_bytes[14], guid_bytes[15]
            );

            // Read type
            let reference_type = reader.read_u32().await?;

            // Read path
            let path = reader.read_null_terminated_string().await?;

            // Convert string GUID to byte array
            let mut guid_bytes = [0u8; 16];
            if guid.len() >= 32 {
                // Assume hex string format
                for i in 0..16 {
                    if let Ok(byte) = u8::from_str_radix(&guid[i * 2..i * 2 + 2], 16) {
                        guid_bytes[i] = byte;
                    }
                }
            }

            externals.push(ExternalReference {
                guid: guid_bytes,
                type_: reference_type as i32,
                path_name: path,
            });
        }

        Ok(externals)
    }

    /// Process individual object
    async fn process_object(
        object_info: ObjectInfo,
        type_tree: Option<AsyncTypeTree>,
        context: Arc<RwLock<AsyncProcessingContext>>,
        _config: AssetConfig,
    ) -> Result<AsyncUnityClass> {
        // This would read the actual object data and deserialize it
        // For now, create a basic AsyncUnityClass

        let class_name = Self::get_class_name(object_info.class_id);

        // Create placeholder data - in real implementation this would parse the binary data
        let mut properties = HashMap::new();
        properties.insert(
            "m_PathID".to_string(),
            UnityValue::Int64(object_info.path_id as i64),
        );
        properties.insert(
            "m_ClassID".to_string(),
            UnityValue::UInt32(object_info.class_id),
        );

        // If we have TypeTree, we could use it to properly deserialize the object
        if let Some(_type_tree) = type_tree {
            // Use TypeTree to deserialize object data
            // This would involve reading the binary data at object_info.byte_offset
            // and using the TypeTree to interpret the structure
        }

        // Create AsyncUnityClass with the corrected structure
        let mut unity_class = AsyncUnityClass::new(
            object_info.class_id as i32,
            class_name.to_string(),
            format!("path_{}", object_info.path_id), // Use path_id as anchor for binary format
        );

        // Set path_id for binary format
        unity_class.path_id = Some(object_info.path_id as i64);

        // Set properties from parsed data
        *unity_class.properties_mut() = properties.into_iter().collect();

        Ok(unity_class)
    }

    /// Get class name from class ID
    fn get_class_name(class_id: u32) -> &'static str {
        match class_id {
            1 => "GameObject",
            4 => "Transform",
            21 => "Material",
            23 => "MeshRenderer",
            25 => "MeshFilter",
            33 => "MeshCollider",
            43 => "Mesh",
            74 => "AnimationClip",
            83 => "AudioClip",
            108 => "Behaviour",
            114 => "MonoBehaviour",
            128 => "Font",
            212 => "SpriteRenderer",
            213 => "Sprite",
            28 => "Texture2D",
            _ => "UnknownClass",
        }
    }
}

/// Asset processing configuration
#[derive(Debug, Clone)]
pub struct AssetConfig {
    /// Maximum concurrent object processing
    pub max_concurrent_objects: usize,
    /// Buffer size for streaming
    pub buffer_size: usize,
    /// Whether to load TypeTree data
    pub load_type_tree: bool,
    /// Whether to cache object data
    pub cache_objects: bool,
}

impl Default for AssetConfig {
    fn default() -> Self {
        Self {
            max_concurrent_objects: 16,
            buffer_size: 65536,
            load_type_tree: true,
            cache_objects: false,
        }
    }
}

/// SerializedFile header information
#[derive(Debug, Clone)]
pub struct SerializedFileHeader {
    pub metadata_size: u32,
    pub file_size: u32,
    pub version: u32,
    pub data_offset: u32,
    pub is_big_endian: bool,
    pub unity_version: UnityVersionInfo,
    pub target_platform: u32,
    pub has_type_tree: bool,
}

/// TypeTree class definition
#[derive(Debug, Clone)]
pub struct TypeTreeClass {
    pub class_id: u32,
    pub nodes: Vec<TypeTreeNode>,
    pub script_type_index: Option<u32>,
}

/// TypeTree node definition
#[derive(Debug, Clone)]
pub struct TypeTreeNode {
    pub type_name: String,
    pub field_name: String,
    pub size: u32,
    pub index: u32,
    pub flags: u32,
    pub version: u32,
    pub meta_flags: u32,
    pub children: Vec<TypeTreeNode>,
}

impl TypeTreeNode {
    /// Check if node is an array
    pub fn is_array(&self) -> bool {
        self.flags & 0x4000 != 0
    }

    /// Check if node is aligned
    pub fn is_aligned(&self) -> bool {
        self.flags & 0x2000 != 0
    }

    /// Get node depth level
    pub fn depth(&self) -> u32 {
        (self.flags >> 24) & 0xFF
    }
}

/// Object information within SerializedFile
#[derive(Debug, Clone)]
pub struct ObjectInfo {
    /// Path ID (unique within file)
    pub path_id: u64,
    /// Byte offset in data section
    pub byte_offset: u64,
    /// Size in bytes
    pub byte_size: u64,
    /// Unity class ID
    pub class_id: u32,
}

// ExternalReference is now an alias to FileIdentifier (defined above)

/// Asset metadata summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetMetadata {
    pub unity_version: UnityVersionInfo,
    pub target_platform: u32,
    pub object_count: usize,
    pub has_type_tree: bool,
    pub external_count: usize,
}

/// High-level async asset processor
pub struct AsyncAsset {
    /// Underlying SerializedFile
    serialized_file: SerializedFile,
    /// Asset name
    name: String,
}

impl AsyncAsset {
    /// Load asset from path
    pub async fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let name = path_ref
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let serialized_file = SerializedFile::load_from_path(path).await?;

        Ok(Self {
            serialized_file,
            name,
        })
    }

    /// Get asset name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get asset metadata
    pub fn metadata(&self) -> AssetMetadata {
        self.serialized_file.metadata()
    }

    /// Stream objects from this asset
    pub fn objects_stream(&self) -> impl Stream<Item = Result<AsyncUnityClass>> + Send + '_ {
        self.serialized_file.objects_stream()
    }

    /// Get specific object by path ID
    pub async fn get_object(&self, path_id: u64) -> Result<Option<AsyncUnityClass>> {
        // This is a simplified search - in practice you'd want indexing
        use futures::StreamExt;
        let mut stream = Box::pin(self.objects_stream());
        while let Some(result) = stream.next().await {
            let obj = result?;
            if obj.path_id == Some(path_id as i64) {
                return Ok(Some(obj));
            }
        }

        Ok(None)
    }

    /// Get all objects of a specific class
    pub async fn get_objects_by_class(&self, class_name: &str) -> Result<Vec<AsyncUnityClass>> {
        let mut objects = Vec::new();

        use futures::StreamExt;
        let mut stream = Box::pin(self.objects_stream());
        while let Some(result) = stream.next().await {
            let obj = result?;
            if obj.class_name == class_name {
                objects.push(obj);
            }
        }

        Ok(objects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test;

    #[tokio::test]
    async fn test_asset_config_defaults() {
        let config = AssetConfig::default();
        assert_eq!(config.max_concurrent_objects, 16);
        assert_eq!(config.buffer_size, 65536);
        assert!(config.load_type_tree);
    }

    #[test]
    fn test_class_name_mapping() {
        assert_eq!(SerializedFile::get_class_name(1), "GameObject");
        assert_eq!(SerializedFile::get_class_name(83), "AudioClip");
        assert_eq!(SerializedFile::get_class_name(28), "Texture2D");
        assert_eq!(SerializedFile::get_class_name(999), "UnknownClass");
    }

    #[test]
    fn test_type_tree_node_flags() {
        let node = TypeTreeNode {
            type_name: "Test".to_string(),
            field_name: "test".to_string(),
            size: 4,
            index: 0,
            flags: 0x4000, // Array flag
            version: 1,
            meta_flags: 0,
            children: Vec::new(),
        };

        assert!(node.is_array());
        assert!(!node.is_aligned());
        assert_eq!(node.depth(), 0);
    }

    #[tokio::test]
    async fn test_object_info() {
        let object_info = ObjectInfo {
            path_id: 123,
            byte_offset: 1000,
            byte_size: 256,
            class_id: 83, // AudioClip
        };

        assert_eq!(object_info.path_id, 123);
        assert_eq!(object_info.class_id, 83);
    }
}

impl SerializedFile {
    /// Read Unity version string (stub implementation)
    async fn read_unity_version<R: AsyncBinaryReader>(
        _reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<String> {
        // TODO: Implement proper Unity version reading
        Ok("2022.3.0f1".to_string())
    }

    /// Read target platform (stub implementation)
    async fn read_target_platform<R: AsyncBinaryReader>(
        _reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<i32> {
        // TODO: Implement proper target platform reading
        Ok(5) // Default to StandaloneWindows
    }

    /// Read type information (stub implementation)
    async fn read_types<R: AsyncBinaryReader>(
        _reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<Vec<SerializedType>> {
        // TODO: Implement proper type reading
        Ok(Vec::new())
    }

    /// Read script types (stub implementation)
    async fn read_script_types<R: AsyncBinaryReader>(
        _reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<Vec<SerializedType>> {
        // TODO: Implement proper script type reading
        Ok(Vec::new())
    }

    /// Read reference types (stub implementation)
    async fn read_ref_types<R: AsyncBinaryReader>(
        _reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<Vec<SerializedType>> {
        // TODO: Implement proper reference type reading
        Ok(Vec::new())
    }

    /// Read user information (stub implementation)
    async fn read_user_information<R: AsyncBinaryReader>(
        _reader: &mut R,
        _header: &SerializedFileHeader,
    ) -> Result<String> {
        // TODO: Implement proper user information reading
        Ok(String::new())
    }
}
