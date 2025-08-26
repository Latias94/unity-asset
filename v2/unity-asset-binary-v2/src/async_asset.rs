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
        let mut serialized_file =
            Self::load_from_reader(stream_reader, AssetConfig::default()).await?;

        // Store the raw data for object extraction (like V1 does)
        serialized_file.data = data;

        // Now populate object data from the stored file data
        serialized_file.populate_object_data().await?;

        Ok(serialized_file)
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
        let types = Self::read_types(&mut reader, &header, enable_type_tree).await?;
        let big_id_enabled = header.version >= 14;

        // Read object information
        let objects = Self::read_object_info(&mut reader, &header).await?;

        // Read script types
        let script_types = Self::read_script_types(&mut reader, &header, enable_type_tree).await?;

        // Read external references
        let externals = Self::read_externals(&mut reader, &header).await?;

        // Read reference types
        let ref_types = Self::read_ref_types(&mut reader, &header, enable_type_tree).await?;

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

        // Create empty nodes for now - would be populated from actual TypeTree data
        let nodes = Vec::new();

        Ok(AsyncTypeTree {
            nodes,
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

    /// Read external references (based on V1 implementation)
    async fn read_externals<R>(
        reader: &mut R,
        header: &SerializedFileHeader,
    ) -> Result<Vec<FileIdentifier>>
    where
        R: AsyncBinaryReader,
    {
        // Read external count
        let external_count = reader.read_u32().await? as usize;
        let mut externals = Vec::with_capacity(external_count);

        for _ in 0..external_count {
            let external = Self::read_file_identifier(reader, header.version).await?;
            externals.push(external);
        }

        Ok(externals)
    }

    /// Read FileIdentifier (based on V1 implementation)
    async fn read_file_identifier<R: AsyncBinaryReader>(
        reader: &mut R,
        version: u32,
    ) -> Result<FileIdentifier> {
        let mut file_identifier = FileIdentifier::new();

        // Read GUID (16 bytes)
        let guid_bytes = reader.read_exact_bytes(16).await?;
        file_identifier.guid.copy_from_slice(&guid_bytes);

        // Read type
        file_identifier.type_ = reader.read_i32().await?;

        // Read path name
        file_identifier.path_name = reader.read_null_terminated_string().await?;

        Ok(file_identifier)
    }

    /// Process individual object (based on V1 implementation)
    async fn process_object(
        object_info: ObjectInfo,
        type_tree: Option<AsyncTypeTree>,
        context: Arc<RwLock<AsyncProcessingContext>>,
        config: AssetConfig,
    ) -> Result<AsyncUnityClass> {
        let class_name = Self::get_class_name(object_info.class_id);

        // Extract actual object data (like V1 does)
        // For now, create empty data - would be populated from file data
        let object_data: Vec<u8> = Vec::new();

        // Create AsyncUnityClass with proper initialization
        let mut unity_class = AsyncUnityClass::new(
            object_info.class_id as i32,
            class_name.to_string(),
            format!("&{}", object_info.path_id), // Use Unity-style anchor format
        );

        // Set path_id for binary format
        unity_class.path_id = Some(object_info.path_id as i64);

        // Parse object data using TypeTree if available
        let mut properties = HashMap::new();

        if let Some(_type_tree) = type_tree {
            if !object_data.is_empty() {
                // TODO: Implement proper TypeTree-based object parsing
                // Current implementation skips TypeTree parsing entirely
                // Full implementation would need to:
                // - Use AsyncObjectProcessor to parse binary data with TypeTree
                // - Handle different Unity data types and structures
                // - Support nested objects and arrays
                // - Handle version-specific parsing differences

                // TODO: Implement parse_object_data method in AsyncObjectProcessor
                // let processor = crate::object_processor::AsyncObjectProcessor::new();
                // match processor.parse_object_data(&object_data, &object_info, &type_tree).await {
                //     Ok(parsed_class) => {
                //         properties = parsed_class.properties().clone();
                //     }
                //     Err(e) => {
                //         eprintln!("Failed to parse object data with TypeTree: {}", e);
                //     }
                // }
            }
        }

        // Always include basic metadata
        properties.insert(
            "m_PathID".to_string(),
            UnityValue::Int64(object_info.path_id as i64),
        );
        properties.insert(
            "m_ClassID".to_string(),
            UnityValue::UInt32(object_info.class_id),
        );

        // Set the parsed properties
        *unity_class.properties_mut() = properties.into_iter().collect();

        // Update processing context
        {
            let mut ctx = context.write().await;
            ctx.stats.objects_processed += 1;
        }

        Ok(unity_class)
    }

    /// Populate object data from stored file data (based on V1 implementation)
    async fn populate_object_data(&mut self) -> Result<()> {
        // TODO: Implement proper object data population from file data
        // Current implementation is a placeholder that doesn't actually extract object data
        // Full implementation would need to:
        // 1. Extract object data from the file data using byte_offset and byte_size
        // 2. Associate TypeTree information with each object
        // 3. Store the data for later processing
        // 4. Handle different Unity versions and their object layouts
        // 5. Support compressed object data

        // TODO: Extend ObjectInfo struct to include data and type_tree fields
        // The actual implementation would require extending ObjectInfo
        // to include data and type_tree fields like V1 does

        Ok(())
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
    /// Create a new TypeTreeNode
    pub fn new() -> Self {
        Self {
            type_name: String::new(),
            field_name: String::new(),
            size: 0,
            index: 0,
            flags: 0,
            version: 0,
            meta_flags: 0,
            children: Vec::new(),
        }
    }

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
    /// Read Unity version string (based on V1 implementation)
    async fn read_unity_version<R: AsyncBinaryReader>(
        reader: &mut R,
        header: &SerializedFileHeader,
    ) -> Result<String> {
        // Read Unity version (if version >= 7)
        if header.version >= 7 {
            reader.read_null_terminated_string().await
        } else {
            Ok(String::new())
        }
    }

    /// Read target platform (based on V1 implementation)
    async fn read_target_platform<R: AsyncBinaryReader>(
        reader: &mut R,
        header: &SerializedFileHeader,
    ) -> Result<i32> {
        // Read target platform (if version >= 8)
        if header.version >= 8 {
            reader.read_i32().await
        } else {
            Ok(0) // Default platform
        }
    }

    /// Read type information (based on V1 implementation)
    async fn read_types<R: AsyncBinaryReader>(
        reader: &mut R,
        header: &SerializedFileHeader,
        enable_type_tree: bool,
    ) -> Result<Vec<SerializedType>> {
        // Read types
        let type_count = reader.read_u32().await? as usize;
        let mut types = Vec::with_capacity(type_count);

        for _ in 0..type_count {
            let serialized_type =
                Self::read_serialized_type(reader, header.version, enable_type_tree).await?;
            types.push(serialized_type);
        }

        Ok(types)
    }

    /// Read SerializedType from binary data (based on V1 implementation)
    async fn read_serialized_type<R: AsyncBinaryReader>(
        reader: &mut R,
        version: u32,
        enable_type_tree: bool,
    ) -> Result<SerializedType> {
        let class_id = reader.read_i32().await?;
        let mut serialized_type = SerializedType::new(class_id);

        if version >= 16 {
            serialized_type.is_stripped_type = reader.read_u8().await? != 0;
        }

        if version >= 17 {
            let script_type_index = reader.read_i32().await? as i16;
            serialized_type.script_type_index = Some(script_type_index);
        }

        if version >= 13 {
            // Based on V1 logic: check conditions for script_id
            let should_read_script_id = if version < 16 {
                class_id < 0
            } else {
                class_id == 114 // MonoBehaviour
            };

            if should_read_script_id {
                // Read script ID (16 bytes)
                let script_id_bytes = reader.read_exact_bytes(16).await?;
                serialized_type.script_id.copy_from_slice(&script_id_bytes);
            }

            // Always read old type hash for version >= 13 (16 bytes)
            let old_type_hash_bytes = reader.read_exact_bytes(16).await?;
            serialized_type
                .old_type_hash
                .copy_from_slice(&old_type_hash_bytes);
        }

        // Read TypeTree if enabled
        if enable_type_tree {
            serialized_type.type_tree = Self::read_type_tree_for_type(reader, version).await?;
        }

        // Read additional fields for version >= 21
        if version >= 21 {
            serialized_type.class_name = reader.read_null_terminated_string().await?;
            serialized_type.namespace = reader.read_null_terminated_string().await?;
            serialized_type.assembly_name = reader.read_null_terminated_string().await?;
        }

        Ok(serialized_type)
    }

    /// Read TypeTree for a specific type (based on V1 implementation)
    async fn read_type_tree_for_type<R: AsyncBinaryReader>(
        reader: &mut R,
        version: u32,
    ) -> Result<TypeTree> {
        let mut type_tree = TypeTree::new();
        type_tree.version = version;

        // Choose parsing method based on version
        if version >= 12 || version == 10 {
            // Use blob format
            Self::read_type_tree_blob(reader, &mut type_tree).await?;
        } else {
            // Use legacy format
            Self::read_type_tree_legacy(reader, &mut type_tree).await?;
        }

        Ok(type_tree)
    }

    /// Read TypeTree in blob format (Unity version >= 12 or == 10)
    async fn read_type_tree_blob<R: AsyncBinaryReader>(
        reader: &mut R,
        type_tree: &mut TypeTree,
    ) -> Result<()> {
        // Read number of nodes
        let node_count = reader.read_i32().await? as usize;

        // Read string buffer size
        let string_buffer_size = reader.read_i32().await? as usize;

        // Read nodes in blob format
        for _ in 0..node_count {
            let mut node = crate::binary_types::TypeTreeNode::new();

            // Read node data in blob format (based on V1)
            node.version = reader.read_u32().await? as i32;
            node.level = reader.read_u8().await? as i32;
            node.type_flags = reader.read_u8().await? as i32;
            node.type_str_offset = reader.read_u32().await?;
            node.name_str_offset = reader.read_u32().await?;
            node.byte_size = reader.read_i32().await?;
            node.index = reader.read_i32().await?;
            node.meta_flags = reader.read_i32().await?;

            if type_tree.version >= 19 {
                node.ref_type_hash = reader.read_u64().await?;
            }

            type_tree.nodes.push(node);
        }

        // Read string buffer
        type_tree.string_buffer = reader.read_exact_bytes(string_buffer_size).await?.to_vec();

        // Resolve string references and build hierarchy
        type_tree.resolve_strings()?;
        type_tree.build_hierarchy()?;

        Ok(())
    }

    /// Read TypeTree in legacy format (Unity version < 12 and != 10)
    async fn read_type_tree_legacy<R: AsyncBinaryReader>(
        reader: &mut R,
        type_tree: &mut TypeTree,
    ) -> Result<()> {
        // Read number of nodes
        let node_count = reader.read_u32().await? as usize;

        // Read string buffer size
        let string_buffer_size = reader.read_u32().await? as usize;

        // Read nodes in legacy format
        for _ in 0..node_count {
            let mut node = crate::binary_types::TypeTreeNode::new();

            // Read type name
            node.type_name = reader.read_null_terminated_string().await?;

            // Read field name
            node.name = reader.read_null_terminated_string().await?;

            // Read byte size
            node.byte_size = reader.read_i32().await?;

            // Read variable count (version 2 only)
            if type_tree.version == 2 {
                let _variable_count = reader.read_i32().await?;
            }

            // Read index (not in version 3)
            if type_tree.version != 3 {
                node.index = reader.read_i32().await?;
            }

            // Read type flags
            node.type_flags = reader.read_i32().await?;

            // Read version
            node.version = reader.read_i32().await?;

            // Read meta flags (not in version 3)
            if type_tree.version != 3 {
                node.meta_flags = reader.read_i32().await?;
            }

            type_tree.nodes.push(node);
        }

        // Read string buffer
        type_tree.string_buffer = reader.read_exact_bytes(string_buffer_size).await?.to_vec();

        // Build hierarchy
        type_tree.build_hierarchy()?;

        Ok(())
    }

    /// Read script types (based on V1 implementation)
    async fn read_script_types<R: AsyncBinaryReader>(
        reader: &mut R,
        header: &SerializedFileHeader,
        enable_type_tree: bool,
    ) -> Result<Vec<SerializedType>> {
        // Read script types (if version >= 11)
        if header.version >= 11 {
            let script_count = reader.read_u32().await? as usize;
            let mut script_types = Vec::with_capacity(script_count);

            for _ in 0..script_count {
                let script_type =
                    Self::read_serialized_type(reader, header.version, enable_type_tree).await?;
                script_types.push(script_type);
            }

            Ok(script_types)
        } else {
            Ok(Vec::new())
        }
    }

    /// Read reference types (based on V1 implementation)
    async fn read_ref_types<R: AsyncBinaryReader>(
        reader: &mut R,
        header: &SerializedFileHeader,
        enable_type_tree: bool,
    ) -> Result<Vec<SerializedType>> {
        // Read ref types (if version >= 20)
        if header.version >= 20 {
            let ref_type_count = reader.read_u32().await? as usize;
            let mut ref_types = Vec::with_capacity(ref_type_count);

            for _ in 0..ref_type_count {
                let ref_type =
                    Self::read_serialized_type(reader, header.version, enable_type_tree).await?;
                ref_types.push(ref_type);
            }

            Ok(ref_types)
        } else {
            Ok(Vec::new())
        }
    }

    /// Read user information (based on V1 implementation)
    async fn read_user_information<R: AsyncBinaryReader>(
        reader: &mut R,
        header: &SerializedFileHeader,
    ) -> Result<String> {
        // Read user information (if version >= 5)
        if header.version >= 5 {
            reader.read_null_terminated_string().await
        } else {
            Ok(String::new())
        }
    }
}
