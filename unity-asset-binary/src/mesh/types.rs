//! Mesh type definitions
//!
//! This module defines all the data structures used for Unity Mesh processing.

use serde::{Deserialize, Serialize};

/// Vertex data structure
/// 
/// Contains information about vertex layout and data for a mesh.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VertexData {
    pub vertex_count: u32,
    pub channels: Vec<ChannelInfo>,
    pub data_size: Vec<u8>,
}

/// Channel information for vertex data
/// 
/// Describes how vertex attributes are laid out in the vertex buffer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub stream: u8,
    pub offset: u8,
    pub format: u8,
    pub dimension: u8,
}

/// SubMesh data structure
/// 
/// Represents a portion of a mesh that uses the same material.
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
/// 
/// Defines the spatial bounds of a mesh or submesh.
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
/// 
/// Contains morph target information for mesh animation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlendShapeData {
    pub vertices: Vec<BlendShapeVertex>,
    pub shapes: Vec<BlendShape>,
    pub channels: Vec<BlendShapeChannel>,
    pub full_weights: Vec<f32>,
}

/// Blend shape vertex
/// 
/// Represents a vertex delta for blend shape animation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlendShapeVertex {
    pub vertex: [f32; 3],
    pub normal: [f32; 3],
    pub tangent: [f32; 3],
    pub index: u32,
}

/// Blend shape
/// 
/// Defines a morph target shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlendShape {
    pub first_vertex: u32,
    pub vertex_count: u32,
    pub has_normals: bool,
    pub has_tangents: bool,
}

/// Blend shape channel
/// 
/// Named channel for blend shape animation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlendShapeChannel {
    pub name: String,
    pub name_hash: u32,
    pub frame_index: i32,
    pub frame_count: i32,
}

/// Streaming info for external mesh data
/// 
/// Information about mesh data stored in external files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamingInfo {
    pub offset: u64,
    pub size: u32,
    pub path: String,
}

/// Compressed mesh data
/// 
/// Contains compressed vertex and index data for memory efficiency.
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
/// 
/// Compressed floating-point data with quantization information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackedFloatVector {
    pub num_items: u32,
    pub range: f32,
    pub start: f32,
    pub data: Vec<u8>,
    pub bit_size: u8,
}

/// Packed int vector for compressed data
/// 
/// Compressed integer data with bit packing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackedIntVector {
    pub num_items: u32,
    pub data: Vec<u8>,
    pub bit_size: u8,
}

/// Mesh object representation
/// 
/// Main mesh structure containing all mesh data and metadata.
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

/// Mesh processing configuration
#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// Whether to extract vertex data
    pub extract_vertices: bool,
    /// Whether to extract index data
    pub extract_indices: bool,
    /// Whether to process blend shapes
    pub process_blend_shapes: bool,
    /// Whether to decompress compressed meshes
    pub decompress_meshes: bool,
    /// Maximum vertex count to process
    pub max_vertex_count: Option<u32>,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            extract_vertices: true,
            extract_indices: true,
            process_blend_shapes: true,
            decompress_meshes: true,
            max_vertex_count: None,
        }
    }
}

/// Mesh processing result
#[derive(Debug, Clone)]
pub struct MeshResult {
    pub mesh: Mesh,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl MeshResult {
    pub fn new(mesh: Mesh) -> Self {
        Self {
            mesh,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn add_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    pub fn add_error(&mut self, error: String) {
        self.errors.push(error);
    }

    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Mesh information summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshInfo {
    pub name: String,
    pub vertex_count: u32,
    pub sub_mesh_count: u32,
    pub triangle_count: u32,
    pub has_blend_shapes: bool,
    pub is_readable: bool,
    pub is_compressed: bool,
    pub has_streaming_data: bool,
}

/// Helper functions for mesh types
impl Mesh {
    /// Get total vertex count
    pub fn vertex_count(&self) -> u32 {
        self.vertex_data.vertex_count
    }

    /// Get total triangle count
    pub fn triangle_count(&self) -> u32 {
        self.sub_meshes.iter().map(|sm| sm.triangle_count).sum()
    }

    /// Check if mesh has blend shapes
    pub fn has_blend_shapes(&self) -> bool {
        self.blend_shape_data.is_some()
    }

    /// Check if mesh is compressed
    pub fn is_compressed(&self) -> bool {
        self.compressed_mesh.is_some()
    }

    /// Check if mesh has streaming data
    pub fn has_streaming_data(&self) -> bool {
        self.stream_data.is_some()
    }

    /// Get mesh bounds
    pub fn bounds(&self) -> &AABB {
        &self.local_aabb
    }

    /// Get mesh information summary
    pub fn get_info(&self) -> MeshInfo {
        MeshInfo {
            name: self.name.clone(),
            vertex_count: self.vertex_count(),
            sub_mesh_count: self.sub_meshes.len() as u32,
            triangle_count: self.triangle_count(),
            has_blend_shapes: self.has_blend_shapes(),
            is_readable: self.is_readable,
            is_compressed: self.is_compressed(),
            has_streaming_data: self.has_streaming_data(),
        }
    }
}

impl AABB {
    /// Create a new AABB
    pub fn new(center: [f32; 3], extent: [f32; 3]) -> Self {
        Self {
            center_x: center[0],
            center_y: center[1],
            center_z: center[2],
            extent_x: extent[0],
            extent_y: extent[1],
            extent_z: extent[2],
        }
    }

    /// Get center as array
    pub fn center(&self) -> [f32; 3] {
        [self.center_x, self.center_y, self.center_z]
    }

    /// Get extent as array
    pub fn extent(&self) -> [f32; 3] {
        [self.extent_x, self.extent_y, self.extent_z]
    }

    /// Get minimum point
    pub fn min(&self) -> [f32; 3] {
        [
            self.center_x - self.extent_x,
            self.center_y - self.extent_y,
            self.center_z - self.extent_z,
        ]
    }

    /// Get maximum point
    pub fn max(&self) -> [f32; 3] {
        [
            self.center_x + self.extent_x,
            self.center_y + self.extent_y,
            self.center_z + self.extent_z,
        ]
    }

    /// Get volume
    pub fn volume(&self) -> f32 {
        8.0 * self.extent_x * self.extent_y * self.extent_z
    }
}

impl SubMesh {
    /// Check if submesh has valid data
    pub fn is_valid(&self) -> bool {
        self.vertex_count > 0 && self.index_count > 0
    }

    /// Get topology name
    pub fn topology_name(&self) -> &'static str {
        match self.topology {
            0 => "Triangles",
            1 => "Quads",
            2 => "Lines",
            3 => "LineStrip",
            4 => "Points",
            _ => "Unknown",
        }
    }
}
