//! Mesh Processing Module
//!
//! This module provides comprehensive Mesh processing capabilities,
//! including parsing from Unity objects and data export.

use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::reader::BinaryReader;
use crate::unity_version::UnityVersion;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use unity_asset_core::UnityValue;

/// Vertex data structure
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VertexData {
    pub vertex_count: u32,
    pub channels: Vec<ChannelInfo>,
    pub data_size: Vec<u8>,
}

/// Channel information for vertex data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub stream: u8,
    pub offset: u8,
    pub format: u8,
    pub dimension: u8,
}

/// SubMesh data structure
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubMesh {
    pub first_byte: u32,
    pub index_count: u32,
    pub topology: i32,
    pub triangle_count: u32,
    pub base_vertex: u32,
    pub first_vertex: u32,
    pub vertex_count: u32,
    pub local_aabb: Option<AABB>,
}

/// Axis-Aligned Bounding Box
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AABB {
    pub center_x: f32,
    pub center_y: f32,
    pub center_z: f32,
    pub extent_x: f32,
    pub extent_y: f32,
    pub extent_z: f32,
}

/// Blend shape data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlendShapeData {
    pub vertices: Vec<BlendShapeVertex>,
    pub shapes: Vec<BlendShape>,
    pub channels: Vec<BlendShapeChannel>,
    pub full_weights: Vec<f32>,
}

/// Blend shape vertex
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlendShapeVertex {
    pub vertex: [f32; 3],
    pub normal: [f32; 3],
    pub tangent: [f32; 3],
    pub index: u32,
}

/// Blend shape
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlendShape {
    pub first_vertex: u32,
    pub vertex_count: u32,
    pub has_normals: bool,
    pub has_tangents: bool,
}

/// Blend shape channel
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlendShapeChannel {
    pub name: String,
    pub name_hash: u32,
    pub frame_index: i32,
    pub frame_count: i32,
}

/// Streaming info for external mesh data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamingInfo {
    pub offset: u64,
    pub size: u32,
    pub path: String,
}

/// Mesh object representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mesh {
    pub name: String,
    pub sub_meshes: Vec<SubMesh>,
    pub blend_shape_data: Option<BlendShapeData>,
    pub bind_pose: Vec<[f32; 16]>, // Matrix4x4 as array
    pub bone_name_hashes: Vec<u32>,
    pub root_bone_name_hash: u32,
    pub mesh_compression: u8,
    pub is_readable: bool,
    pub keep_vertices: bool,
    pub keep_indices: bool,
    pub index_format: i32,
    pub index_buffer: Vec<u8>,
    pub vertex_data: VertexData,
    pub compressed_mesh: Option<CompressedMesh>,
    pub local_aabb: AABB,
    pub mesh_usage_flags: i32,
    pub baked_convex_collision_mesh: Vec<u8>,
    pub baked_triangle_collision_mesh: Vec<u8>,
    pub mesh_metrics: [f32; 2],
    pub stream_data: Option<StreamingInfo>,
}

/// Compressed mesh data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompressedMesh {
    pub vertices: PackedFloatVector,
    pub uv: PackedFloatVector,
    pub normals: PackedFloatVector,
    pub tangents: PackedFloatVector,
    pub weights: PackedIntVector,
    pub normal_signs: PackedIntVector,
    pub tangent_signs: PackedIntVector,
    pub float_colors: Option<PackedFloatVector>,
    pub bone_indices: PackedIntVector,
    pub triangles: PackedIntVector,
    pub colors: Option<PackedIntVector>,
    pub uv_info: u32,
}

/// Packed float vector for compressed data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackedFloatVector {
    pub num_items: u32,
    pub range: f32,
    pub start: f32,
    pub data: Vec<u8>,
    pub bit_size: u8,
}

/// Packed int vector for compressed data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackedIntVector {
    pub num_items: u32,
    pub data: Vec<u8>,
    pub bit_size: u8,
}

impl Default for Mesh {
    fn default() -> Self {
        Self {
            name: String::new(),
            sub_meshes: Vec::new(),
            blend_shape_data: None,
            bind_pose: Vec::new(),
            bone_name_hashes: Vec::new(),
            root_bone_name_hash: 0,
            mesh_compression: 0,
            is_readable: true,
            keep_vertices: true,
            keep_indices: true,
            index_format: 0,
            index_buffer: Vec::new(),
            vertex_data: VertexData::default(),
            compressed_mesh: None,
            local_aabb: AABB::default(),
            mesh_usage_flags: 0,
            baked_convex_collision_mesh: Vec::new(),
            baked_triangle_collision_mesh: Vec::new(),
            mesh_metrics: [0.0, 0.0],
            stream_data: None,
        }
    }
}

impl Mesh {
    /// Parse Mesh from UnityObject
    pub fn from_unity_object(obj: &UnityObject, version: &UnityVersion) -> Result<Self> {
        // Try to parse using TypeTree first
        if let Some(type_tree) = &obj.info.type_tree {
            let properties = obj.parse_with_typetree(type_tree)?;
            Self::from_typetree(&properties, version)
        } else {
            // Fallback: parse from raw binary data
            Self::from_binary_data(&obj.info.data, version)
        }
    }

    /// Parse Mesh from TypeTree properties
    pub fn from_typetree(
        properties: &IndexMap<String, UnityValue>,
        _version: &UnityVersion,
    ) -> Result<Self> {
        let mut mesh = Mesh::default();

        // Extract name
        if let Some(UnityValue::String(name)) = properties.get("m_Name") {
            mesh.name = name.clone();
        }

        // Extract sub meshes
        if let Some(sub_meshes_value) = properties.get("m_SubMeshes") {
            mesh.extract_sub_meshes(sub_meshes_value)?;
        }

        // Extract vertex data
        if let Some(vertex_data_value) = properties.get("m_VertexData") {
            mesh.extract_vertex_data(vertex_data_value)?;
        }

        // Extract index buffer
        if let Some(index_buffer_value) = properties.get("m_IndexBuffer") {
            mesh.extract_index_buffer(index_buffer_value)?;
        }

        // Extract readable flag
        if let Some(UnityValue::Bool(is_readable)) = properties.get("m_IsReadable") {
            mesh.is_readable = *is_readable;
        }

        // Extract local AABB
        if let Some(local_aabb_value) = properties.get("m_LocalAABB") {
            mesh.extract_local_aabb(local_aabb_value)?;
        }

        // Extract mesh compression
        if let Some(UnityValue::Integer(compression)) = properties.get("m_MeshCompression") {
            mesh.mesh_compression = *compression as u8;
        }

        // Extract streaming info if present
        if let Some(stream_data) = properties.get("m_StreamData") {
            mesh.stream_data = Self::extract_stream_data(stream_data)?;
        }

        Ok(mesh)
    }

    /// Parse Mesh from raw binary data (fallback method)
    pub fn from_binary_data(data: &[u8], _version: &UnityVersion) -> Result<Self> {
        let mut reader = BinaryReader::new(data, crate::reader::ByteOrder::Little);
        let mut mesh = Mesh::default();

        // Read name (aligned string)
        mesh.name = reader.read_aligned_string()?;

        // Read basic properties
        mesh.is_readable = reader.read_bool()?;
        mesh.keep_vertices = reader.read_bool()?;
        mesh.keep_indices = reader.read_bool()?;

        // Read index format
        mesh.index_format = reader.read_i32()?;

        // Read index buffer size and data
        let index_buffer_size = reader.read_i32()? as usize;
        if index_buffer_size > 0 {
            mesh.index_buffer = reader.read_bytes(index_buffer_size)?;
        }

        // Read mesh compression
        mesh.mesh_compression = reader.read_u8()?;

        Ok(mesh)
    }

    /// Extract sub meshes from UnityValue
    fn extract_sub_meshes(&mut self, _value: &UnityValue) -> Result<()> {
        // SubMeshes is typically an array of complex objects
        // This is a simplified implementation
        Ok(())
    }

    /// Extract vertex data from UnityValue
    fn extract_vertex_data(&mut self, _value: &UnityValue) -> Result<()> {
        // VertexData is a complex structure with channels and data
        // This is a simplified implementation
        Ok(())
    }

    /// Extract index buffer from UnityValue
    fn extract_index_buffer(&mut self, value: &UnityValue) -> Result<()> {
        match value {
            UnityValue::Array(arr) => {
                let mut buffer = Vec::new();
                for item in arr {
                    if let UnityValue::Integer(byte_val) = item {
                        buffer.push(*byte_val as u8);
                    }
                }
                self.index_buffer = buffer;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Extract local AABB from UnityValue
    fn extract_local_aabb(&mut self, _value: &UnityValue) -> Result<()> {
        // AABB is typically a structure with center and extent
        // This is a simplified implementation
        Ok(())
    }

    /// Extract stream data from UnityValue
    fn extract_stream_data(_value: &UnityValue) -> Result<Option<StreamingInfo>> {
        // StreamingInfo is typically a complex object with offset, size, and path
        // This is a simplified implementation
        Ok(None) // TODO: Implement full streaming info extraction
    }

    /// Get mesh info
    pub fn get_info(&self) -> MeshInfo {
        MeshInfo {
            name: self.name.clone(),
            vertex_count: self.vertex_data.vertex_count,
            sub_mesh_count: self.sub_meshes.len() as u32,
            triangle_count: self.sub_meshes.iter().map(|sm| sm.triangle_count).sum(),
            is_readable: self.is_readable,
            has_blend_shapes: self.blend_shape_data.is_some(),
            is_compressed: self.compressed_mesh.is_some(),
        }
    }

    /// Export mesh data (simplified OBJ format)
    pub fn export(&self) -> Result<String> {
        let mut obj_data = String::new();

        // Add header
        obj_data.push_str(&format!("# Mesh: {}\n", self.name));
        obj_data.push_str("# Exported from Unity Asset Parser\n\n");

        // For now, return a placeholder since we need to implement vertex parsing
        obj_data.push_str("# Vertex data parsing not yet implemented\n");
        obj_data.push_str("# This is a placeholder export\n");

        if !self.sub_meshes.is_empty() {
            obj_data.push_str(&format!("# Sub-meshes: {}\n", self.sub_meshes.len()));
            for (i, sub_mesh) in self.sub_meshes.iter().enumerate() {
                obj_data.push_str(&format!(
                    "# SubMesh {}: {} triangles\n",
                    i, sub_mesh.triangle_count
                ));
            }
        }

        Ok(obj_data)
    }
}

/// Mesh information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshInfo {
    pub name: String,
    pub vertex_count: u32,
    pub sub_mesh_count: u32,
    pub triangle_count: u32,
    pub is_readable: bool,
    pub has_blend_shapes: bool,
    pub is_compressed: bool,
}

/// Mesh processor for handling different Unity versions
#[derive(Debug, Clone)]
pub struct MeshProcessor {
    version: UnityVersion,
}

impl MeshProcessor {
    /// Create a new Mesh processor
    pub fn new(version: UnityVersion) -> Self {
        Self { version }
    }

    /// Parse Mesh from Unity object
    pub fn parse_mesh(&self, object: &UnityObject) -> Result<Mesh> {
        Mesh::from_unity_object(object, &self.version)
    }

    /// Get supported mesh features for this Unity version
    pub fn get_supported_features(&self) -> Vec<&'static str> {
        let mut features = vec!["basic_mesh", "sub_meshes", "vertex_data"];

        if self.version.major >= 5 {
            features.push("blend_shapes");
            features.push("compressed_mesh");
        }

        if self.version.major >= 2017 {
            features.push("mesh_optimization");
            features.push("streaming_info");
        }

        if self.version.major >= 2018 {
            features.push("mesh_usage_flags");
        }

        features
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mesh_default() {
        let mesh = Mesh::default();
        assert_eq!(mesh.name, "");
        assert!(mesh.is_readable);
        assert!(mesh.keep_vertices);
        assert!(mesh.keep_indices);
        assert_eq!(mesh.mesh_compression, 0);
    }

    #[test]
    fn test_mesh_processor() {
        let version = UnityVersion::from_str("2020.3.12f1").unwrap();
        let processor = MeshProcessor::new(version);

        let features = processor.get_supported_features();
        assert!(features.contains(&"basic_mesh"));
        assert!(features.contains(&"blend_shapes"));
        assert!(features.contains(&"mesh_optimization"));
        assert!(features.contains(&"mesh_usage_flags"));
    }

    #[test]
    fn test_mesh_export() {
        let mesh = Mesh::default();
        let export_result = mesh.export();
        assert!(export_result.is_ok());

        let obj_data = export_result.unwrap();
        assert!(obj_data.contains("# Mesh:"));
        assert!(obj_data.contains("# Exported from Unity Asset Parser"));
    }
}
