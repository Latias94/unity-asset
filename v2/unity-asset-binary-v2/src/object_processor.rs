//! Object Processor
//!
//! Async Unity object processing with TypeTree-based deserialization and streaming support.

use crate::async_asset::{AsyncTypeTree, TypeTreeClass, TypeTreeNode};
use crate::binary_types::{AsyncBinaryData, AsyncBinaryReader, StreamPosition};
use crate::stream_reader::AsyncStreamReader;
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use unity_asset_core_v2::{AsyncUnityClass, ObjectMetadata, Result, UnityAssetError, UnityValue};

/// Object processor configuration
#[derive(Debug, Clone)]
pub struct ObjectConfig {
    /// Maximum object size for safety
    pub max_object_size: usize,
    /// Whether to validate object structure
    pub validate_structure: bool,
    /// Whether to cache parsed objects
    pub cache_parsed_objects: bool,
    /// Maximum cache size
    pub max_cache_size: usize,
}

impl Default for ObjectConfig {
    fn default() -> Self {
        Self {
            max_object_size: 100 * 1024 * 1024, // 100MB max object size
            validate_structure: true,
            cache_parsed_objects: false, // Memory conscious by default
            max_cache_size: 1000,        // Max 1000 cached objects
        }
    }
}

/// Async object processor for Unity assets
pub struct AsyncObjectProcessor {
    /// Processing configuration
    config: ObjectConfig,
    /// Cached objects
    object_cache: Arc<RwLock<HashMap<u64, AsyncUnityClass>>>,
    /// Processing statistics
    stats: Arc<RwLock<ObjectProcessingStats>>,
}

impl AsyncObjectProcessor {
    /// Create new object processor
    pub fn new() -> Self {
        Self {
            config: ObjectConfig::default(),
            stats: Arc::new(RwLock::new(ObjectProcessingStats::default())),
            object_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create object processor with configuration
    pub fn with_config(config: ObjectConfig) -> Self {
        Self {
            config,
            stats: Arc::new(RwLock::new(ObjectProcessingStats::default())),
            object_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Process object from binary data
    pub async fn process_object<R>(
        &self,
        reader: &mut R,
        object_info: &ObjectInfo,
        type_tree: Option<&AsyncTypeTree>,
    ) -> Result<AsyncUnityClass>
    where
        R: AsyncBinaryReader,
    {
        // Check cache first if enabled
        if self.config.cache_parsed_objects {
            let cache = self.object_cache.read().await;
            if let Some(cached_object) = cache.get(&object_info.path_id) {
                self.stats.write().await.cache_hits += 1;
                return Ok(cached_object.clone());
            }
        }

        // Validate object size
        if object_info.byte_size as usize > self.config.max_object_size {
            return Err(UnityAssetError::parse_error(
                format!(
                    "Object size {} exceeds maximum {}",
                    object_info.byte_size, self.config.max_object_size
                ),
                0,
            ));
        }

        // Seek to object position
        reader.seek(object_info.byte_offset).await?;

        // Read object data
        let object_data = reader
            .read_exact_bytes(object_info.byte_size as usize)
            .await?;

        // Process the object based on available information
        let unity_class = if let Some(type_tree) = type_tree {
            // Use TypeTree for parsing if available
            self.process_with_type_tree(&object_data, object_info, type_tree)
                .await?
        } else {
            // Fallback to class ID-based parsing
            self.process_with_class_id(&object_data, object_info)
                .await?
        };

        // Update statistics
        {
            let mut stats = self.stats.write().await;
            stats.objects_processed += 1;
            stats.bytes_processed += object_info.byte_size;
        }

        // Cache the result if enabled
        if self.config.cache_parsed_objects {
            let mut cache = self.object_cache.write().await;
            if cache.len() < self.config.max_cache_size {
                cache.insert(object_info.path_id, unity_class.clone());
            }
        }

        Ok(unity_class)
    }

    /// Process object using TypeTree information
    async fn process_with_type_tree(
        &self,
        data: &[u8],
        object_info: &ObjectInfo,
        type_tree: &AsyncTypeTree,
    ) -> Result<AsyncUnityClass> {
        let class_info: Option<&TypeTreeClass> = None; // TODO: Implement TypeTree class lookup
        let class_info = class_info.ok_or_else(|| {
            UnityAssetError::parse_error(
                format!("No TypeTree info for class ID {}", object_info.class_id),
                0,
            )
        })?;

        let mut reader = AsyncStreamReader::new(std::io::Cursor::new(data));
        let parsed_data = self
            .parse_type_tree_nodes(&mut reader, &class_info.nodes)
            .await?;

        let mut unity_class = AsyncUnityClass::with_path_id(
            object_info.class_id as i32,
            Self::get_class_name(object_info.class_id).to_string(),
            format!("&{}", object_info.path_id),
            object_info.path_id as i64,
        );

        // Set additional fields
        unity_class.file_id = "Unknown".to_string();
        unity_class.data = parsed_data;

        Ok(unity_class)
    }

    /// Process object using class ID fallback
    async fn process_with_class_id(
        &self,
        data: &[u8],
        object_info: &ObjectInfo,
    ) -> Result<AsyncUnityClass> {
        let mut reader = AsyncStreamReader::new(std::io::Cursor::new(data));

        // Parse based on known class structure
        let parsed_data = match object_info.class_id {
            1 => self.parse_game_object(&mut reader).await?, // GameObject
            4 => self.parse_transform(&mut reader).await?,   // Transform
            28 => self.parse_texture2d(&mut reader).await?,  // Texture2D
            83 => self.parse_audio_clip(&mut reader).await?, // AudioClip
            43 => self.parse_mesh(&mut reader).await?,       // Mesh
            213 => self.parse_sprite(&mut reader).await?,    // Sprite
            _ => {
                self.parse_unknown_object(&mut reader, object_info.class_id)
                    .await?
            }
        };

        let mut unity_class = AsyncUnityClass::with_path_id(
            object_info.class_id as i32,
            Self::get_class_name(object_info.class_id).to_string(),
            format!("&{}", object_info.path_id),
            object_info.path_id as i64,
        );

        // Set additional fields
        unity_class.file_id = "Unknown".to_string();
        unity_class.data = parsed_data;

        Ok(unity_class)
    }

    /// Parse TypeTree nodes recursively
    async fn parse_type_tree_nodes<R>(
        &self,
        reader: &mut R,
        nodes: &[TypeTreeNode],
    ) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        if nodes.is_empty() {
            return Ok(UnityValue::Null);
        }

        let root_node = &nodes[0];
        self.parse_type_tree_node(reader, root_node).await
    }

    /// Parse individual TypeTree node
    async fn parse_type_tree_node<R>(
        &self,
        reader: &mut R,
        node: &TypeTreeNode,
    ) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        match node.type_name.as_str() {
            "int" | "SInt32" => {
                let value = reader.read_i32().await?;
                Ok(UnityValue::Int32(value))
            }
            "unsigned int" | "UInt32" => {
                let value = reader.read_u32().await?;
                Ok(UnityValue::UInt32(value))
            }
            "long long" | "SInt64" => {
                let value = reader.read_i64().await?;
                Ok(UnityValue::Int64(value))
            }
            "unsigned long long" | "UInt64" => {
                let value = reader.read_u64().await?;
                Ok(UnityValue::UInt64(value))
            }
            "float" => {
                let value = reader.read_f32().await?;
                Ok(UnityValue::Float(value as f64))
            }
            "double" => {
                let value = reader.read_f64().await?;
                Ok(UnityValue::Double(value))
            }
            "bool" => {
                let value = reader.read_u8().await? != 0;
                Ok(UnityValue::Bool(value))
            }
            "string" => {
                let string_value = reader.read_length_prefixed_string().await?;
                Ok(UnityValue::String(string_value))
            }
            "Array" => {
                // Handle array parsing
                Box::pin(self.parse_array(reader, node)).await
            }
            _ => {
                // Handle complex objects
                if !node.children.is_empty() {
                    Box::pin(self.parse_object_from_children(reader, &node.children)).await
                } else {
                    // Unknown primitive type, try to read as bytes
                    if node.size > 0 && node.size <= 1024 {
                        // Reasonable size limit for unknown data
                        let bytes = reader.read_exact_bytes(node.size as usize).await?;
                        Ok(UnityValue::Bytes(bytes.to_vec()))
                    } else {
                        Ok(UnityValue::Null)
                    }
                }
            }
        }
    }

    /// Parse array from TypeTree
    async fn parse_array<R>(
        &self,
        reader: &mut R,
        array_node: &TypeTreeNode,
    ) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        // Read array size
        let size = reader.read_u32().await?;
        let mut elements = Vec::with_capacity(size as usize);

        // Find element type from children
        if let Some(element_node) = array_node.children.get(1) {
            // Children[0] is usually "size", children[1] is "data" containing element type
            for _ in 0..size {
                let element = Box::pin(self.parse_type_tree_node(reader, element_node)).await?;
                elements.push(element);
            }
        }

        Ok(UnityValue::Array(elements))
    }

    /// Parse object from children nodes
    async fn parse_object_from_children<R>(
        &self,
        reader: &mut R,
        children: &[TypeTreeNode],
    ) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        let mut object_properties = HashMap::new();

        for child in children {
            let value = Box::pin(self.parse_type_tree_node(reader, child)).await?;
            object_properties.insert(child.field_name.clone(), value);
        }

        Ok(UnityValue::Object(object_properties.into_iter().collect()))
    }

    /// Parse GameObject structure
    async fn parse_game_object<R>(&self, reader: &mut R) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        let mut properties = HashMap::new();

        // Read basic GameObject properties
        let component_count = reader.read_u32().await?;
        properties.insert(
            "m_ComponentCount".to_string(),
            UnityValue::UInt32(component_count),
        );

        // Read components array
        let mut components = Vec::new();
        for _ in 0..component_count {
            let component_type = reader.read_u32().await?;
            let component_ptr = reader.read_u64().await?;

            let mut component_data = HashMap::new();
            component_data.insert("type".to_string(), UnityValue::UInt32(component_type));
            component_data.insert("ptr".to_string(), UnityValue::UInt64(component_ptr));

            components.push(UnityValue::Object(component_data.into_iter().collect()));
        }
        properties.insert("m_Components".to_string(), UnityValue::Array(components));

        // Read layer
        let layer = reader.read_u32().await?;
        properties.insert("m_Layer".to_string(), UnityValue::UInt32(layer));

        // Read name
        let name = reader.read_length_prefixed_string().await?;
        properties.insert("m_Name".to_string(), UnityValue::String(name));

        Ok(UnityValue::Object(properties.into_iter().collect()))
    }

    /// Parse Transform structure
    async fn parse_transform<R>(&self, reader: &mut R) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        let mut properties = HashMap::new();

        // Read position (Vector3)
        let pos_x = reader.read_f32().await?;
        let pos_y = reader.read_f32().await?;
        let pos_z = reader.read_f32().await?;
        let mut position = HashMap::new();
        position.insert("x".to_string(), UnityValue::Float(pos_x as f64));
        position.insert("y".to_string(), UnityValue::Float(pos_y as f64));
        position.insert("z".to_string(), UnityValue::Float(pos_z as f64));
        properties.insert(
            "m_LocalPosition".to_string(),
            UnityValue::Object(position.into_iter().collect()),
        );

        // Read rotation (Quaternion)
        let rot_x = reader.read_f32().await?;
        let rot_y = reader.read_f32().await?;
        let rot_z = reader.read_f32().await?;
        let rot_w = reader.read_f32().await?;
        let mut rotation = HashMap::new();
        rotation.insert("x".to_string(), UnityValue::Float(rot_x as f64));
        rotation.insert("y".to_string(), UnityValue::Float(rot_y as f64));
        rotation.insert("z".to_string(), UnityValue::Float(rot_z as f64));
        rotation.insert("w".to_string(), UnityValue::Float(rot_w as f64));
        properties.insert(
            "m_LocalRotation".to_string(),
            UnityValue::Object(rotation.into_iter().collect()),
        );

        // Read scale (Vector3)
        let scale_x = reader.read_f32().await?;
        let scale_y = reader.read_f32().await?;
        let scale_z = reader.read_f32().await?;
        let mut scale = HashMap::new();
        scale.insert("x".to_string(), UnityValue::Float(scale_x as f64));
        scale.insert("y".to_string(), UnityValue::Float(scale_y as f64));
        scale.insert("z".to_string(), UnityValue::Float(scale_z as f64));
        properties.insert(
            "m_LocalScale".to_string(),
            UnityValue::Object(scale.into_iter().collect()),
        );

        Ok(UnityValue::Object(properties.into_iter().collect()))
    }

    /// Parse Texture2D structure (basic)
    async fn parse_texture2d<R>(&self, reader: &mut R) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        let mut properties = HashMap::new();

        // Read texture properties
        let width = reader.read_u32().await?;
        let height = reader.read_u32().await?;
        let format = reader.read_i32().await?;
        let mip_count = reader.read_u32().await?;

        properties.insert("m_Width".to_string(), UnityValue::UInt32(width));
        properties.insert("m_Height".to_string(), UnityValue::UInt32(height));
        properties.insert("m_TextureFormat".to_string(), UnityValue::Int32(format));
        properties.insert("m_MipCount".to_string(), UnityValue::UInt32(mip_count));

        // Read texture settings (simplified)
        let is_readable = reader.read_u8().await? != 0;
        properties.insert("m_IsReadable".to_string(), UnityValue::Bool(is_readable));

        Ok(UnityValue::Object(properties.into_iter().collect()))
    }

    /// Parse AudioClip structure (basic)
    async fn parse_audio_clip<R>(&self, reader: &mut R) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        let mut properties = HashMap::new();

        // Read audio properties
        let format = reader.read_i32().await?;
        let frequency = reader.read_u32().await?;
        let channels = reader.read_u32().await?;
        let samples = reader.read_u64().await?;

        properties.insert("m_CompressionFormat".to_string(), UnityValue::Int32(format));
        properties.insert("m_Frequency".to_string(), UnityValue::UInt32(frequency));
        properties.insert("m_Channels".to_string(), UnityValue::UInt32(channels));
        properties.insert("m_Samples".to_string(), UnityValue::UInt64(samples));

        // Read audio settings
        let load_type = reader.read_i32().await?;
        let is_3d = reader.read_u8().await? != 0;

        properties.insert("m_LoadType".to_string(), UnityValue::Int32(load_type));
        properties.insert("m_3D".to_string(), UnityValue::Bool(is_3d));

        Ok(UnityValue::Object(properties.into_iter().collect()))
    }

    /// Parse Mesh structure (basic)
    async fn parse_mesh<R>(&self, reader: &mut R) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        let mut properties = HashMap::new();

        // This is a simplified mesh parser - real mesh parsing is complex
        let vertex_count = reader.read_u32().await?;
        properties.insert(
            "m_VertexCount".to_string(),
            UnityValue::UInt32(vertex_count),
        );

        Ok(UnityValue::Object(properties.into_iter().collect()))
    }

    /// Parse Sprite structure (basic)
    async fn parse_sprite<R>(&self, reader: &mut R) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        let mut properties = HashMap::new();

        // Read sprite rect
        let rect_x = reader.read_f32().await?;
        let rect_y = reader.read_f32().await?;
        let rect_width = reader.read_f32().await?;
        let rect_height = reader.read_f32().await?;

        let mut rect = HashMap::new();
        rect.insert("x".to_string(), UnityValue::Float(rect_x as f64));
        rect.insert("y".to_string(), UnityValue::Float(rect_y as f64));
        rect.insert("width".to_string(), UnityValue::Float(rect_width as f64));
        rect.insert("height".to_string(), UnityValue::Float(rect_height as f64));

        properties.insert(
            "m_Rect".to_string(),
            UnityValue::Object(rect.into_iter().collect()),
        );

        Ok(UnityValue::Object(properties.into_iter().collect()))
    }

    /// Parse unknown object (fallback)
    async fn parse_unknown_object<R>(&self, reader: &mut R, class_id: u32) -> Result<UnityValue>
    where
        R: AsyncBinaryReader,
    {
        let mut properties = HashMap::new();
        properties.insert("m_ClassID".to_string(), UnityValue::UInt32(class_id));

        // Just store the class ID for unknown objects
        Ok(UnityValue::Object(properties.into_iter().collect()))
    }

    /// Get class name from class ID
    fn get_class_name(class_id: u32) -> &'static str {
        match class_id {
            1 => "GameObject",
            4 => "Transform",
            21 => "Material",
            23 => "MeshRenderer",
            25 => "MeshFilter",
            28 => "Texture2D",
            33 => "MeshCollider",
            43 => "Mesh",
            74 => "AnimationClip",
            83 => "AudioClip",
            108 => "Behaviour",
            114 => "MonoBehaviour",
            128 => "Font",
            212 => "SpriteRenderer",
            213 => "Sprite",
            _ => "UnknownClass",
        }
    }

    /// Get processing statistics
    pub async fn stats(&self) -> ObjectProcessingStats {
        self.stats.read().await.clone()
    }

    /// Clear object cache
    pub async fn clear_cache(&self) {
        self.object_cache.write().await.clear();
    }
}

impl Default for AsyncObjectProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// Object information for processing
#[derive(Debug, Clone)]
pub struct ObjectInfo {
    /// Object path ID
    pub path_id: u64,
    /// Offset in file
    pub byte_offset: u64,
    /// Size in bytes
    pub byte_size: u64,
    /// Unity class ID
    pub class_id: u32,
}

/// Object processing statistics
#[derive(Debug, Default, Clone)]
pub struct ObjectProcessingStats {
    /// Number of objects processed
    pub objects_processed: u64,
    /// Total bytes processed
    pub bytes_processed: u64,
    /// Cache hit count
    pub cache_hits: u64,
    /// Processing errors
    pub error_count: u64,
}

impl ObjectProcessingStats {
    /// Calculate cache hit rate
    pub fn cache_hit_rate(&self) -> f64 {
        if self.objects_processed == 0 {
            0.0
        } else {
            self.cache_hits as f64 / self.objects_processed as f64
        }
    }

    /// Calculate error rate
    pub fn error_rate(&self) -> f64 {
        if self.objects_processed == 0 {
            0.0
        } else {
            self.error_count as f64 / self.objects_processed as f64
        }
    }

    /// Calculate average object size
    pub fn average_object_size(&self) -> f64 {
        if self.objects_processed == 0 {
            0.0
        } else {
            self.bytes_processed as f64 / self.objects_processed as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_object_processor_creation() {
        let processor = AsyncObjectProcessor::new();
        let stats = processor.stats().await;
        assert_eq!(stats.objects_processed, 0);
        assert_eq!(stats.cache_hits, 0);
    }

    #[test]
    fn test_class_name_mapping() {
        assert_eq!(AsyncObjectProcessor::get_class_name(1), "GameObject");
        assert_eq!(AsyncObjectProcessor::get_class_name(4), "Transform");
        assert_eq!(AsyncObjectProcessor::get_class_name(28), "Texture2D");
        assert_eq!(AsyncObjectProcessor::get_class_name(83), "AudioClip");
        assert_eq!(AsyncObjectProcessor::get_class_name(999), "UnknownClass");
    }

    #[test]
    fn test_object_info() {
        let obj_info = ObjectInfo {
            path_id: 123,
            byte_offset: 1000,
            byte_size: 256,
            class_id: 28, // Texture2D
        };

        assert_eq!(obj_info.path_id, 123);
        assert_eq!(obj_info.class_id, 28);
        assert_eq!(obj_info.byte_size, 256);
    }

    #[test]
    fn test_processing_stats() {
        let mut stats = ObjectProcessingStats::default();
        stats.objects_processed = 100;
        stats.cache_hits = 25;
        stats.error_count = 5;
        stats.bytes_processed = 10000;

        assert_eq!(stats.cache_hit_rate(), 0.25);
        assert_eq!(stats.error_rate(), 0.05);
        assert_eq!(stats.average_object_size(), 100.0);
    }

    #[test]
    fn test_object_config_defaults() {
        let config = ObjectConfig::default();
        assert_eq!(config.max_object_size, 100 * 1024 * 1024);
        assert!(config.validate_structure);
        assert!(!config.cache_parsed_objects);
        assert_eq!(config.max_cache_size, 1000);
    }
}
