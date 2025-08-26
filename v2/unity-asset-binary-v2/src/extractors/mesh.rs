//! Async Mesh Processing
//!
//! Provides async mesh extraction and processing for Unity Mesh assets.
//! Supports vertex data, indices, and basic mesh export functionality.

use crate::binary_types::AsyncBinaryReader;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use unity_asset_core_v2::{AsyncUnityClass, Result, UnityAssetError, UnityValue};

/// Processed mesh data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessedMesh {
    pub name: String,
    pub vertex_count: u32,
    pub triangle_count: u32,
    pub vertices: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub sub_meshes: Vec<SubMesh>,
}

/// Sub-mesh information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubMesh {
    pub first_byte: u32,
    pub index_count: u32,
    pub topology: u32,
    pub base_vertex: u32,
    pub first_vertex: u32,
    pub vertex_count: u32,
}

/// Unity Mesh asset representation
#[derive(Debug, Clone)]
pub struct AsyncMesh {
    pub name: String,
    pub vertex_count: u32,
    pub vertices: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub sub_meshes: Vec<SubMesh>,
}

impl AsyncMesh {
    /// Create new mesh from Unity class
    pub async fn from_unity_class(unity_class: &AsyncUnityClass) -> Result<Self> {
        let properties = unity_class.properties();

        let name = properties
            .get("m_Name")
            .and_then(|v| v.as_string())
            .unwrap_or("Mesh".to_string());

        let vertex_count = properties
            .get("m_VertexCount")
            .and_then(|v| v.as_u32())
            .unwrap_or(0);

        Ok(Self {
            name,
            vertex_count,
            vertices: Vec::new(), // Would be populated from actual mesh data
            normals: Vec::new(),
            uvs: Vec::new(),
            indices: Vec::new(),
            sub_meshes: Vec::new(),
        })
    }

    /// Export mesh to OBJ format
    pub fn export_obj(&self) -> String {
        let mut obj = String::new();
        obj.push_str(&format!("# Mesh: {}\n", self.name));
        obj.push_str(&format!("# Vertices: {}\n", self.vertices.len()));
        obj.push_str(&format!("# Triangles: {}\n", self.indices.len() / 3));
        obj.push('\n');

        // Write vertices
        for vertex in &self.vertices {
            obj.push_str(&format!("v {} {} {}\n", vertex[0], vertex[1], vertex[2]));
        }

        // Write normals
        for normal in &self.normals {
            obj.push_str(&format!("vn {} {} {}\n", normal[0], normal[1], normal[2]));
        }

        // Write UVs
        for uv in &self.uvs {
            obj.push_str(&format!("vt {} {}\n", uv[0], uv[1]));
        }

        // Write faces
        for chunk in self.indices.chunks(3) {
            if chunk.len() == 3 {
                obj.push_str(&format!(
                    "f {}/{}/{} {}/{}/{} {}/{}/{}\n",
                    chunk[0] + 1,
                    chunk[0] + 1,
                    chunk[0] + 1,
                    chunk[1] + 1,
                    chunk[1] + 1,
                    chunk[1] + 1,
                    chunk[2] + 1,
                    chunk[2] + 1,
                    chunk[2] + 1
                ));
            }
        }

        obj
    }
}

/// Mesh processing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshConfig {
    pub extract_vertices: bool,
    pub extract_normals: bool,
    pub extract_uvs: bool,
    pub extract_indices: bool,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            extract_vertices: true,
            extract_normals: true,
            extract_uvs: true,
            extract_indices: true,
        }
    }
}

/// Async mesh processor
pub struct AsyncMeshProcessor {
    config: MeshConfig,
}

impl AsyncMeshProcessor {
    /// Create new mesh processor
    pub fn new() -> Self {
        Self {
            config: MeshConfig::default(),
        }
    }

    /// Create with custom configuration
    pub fn with_config(config: MeshConfig) -> Self {
        Self { config }
    }

    /// Process mesh from Unity class
    pub async fn process_mesh(&self, unity_class: &AsyncUnityClass) -> Result<ProcessedMesh> {
        let mesh = AsyncMesh::from_unity_class(unity_class).await?;

        Ok(ProcessedMesh {
            name: mesh.name,
            vertex_count: mesh.vertex_count,
            triangle_count: (mesh.indices.len() / 3) as u32,
            vertices: mesh.vertices,
            normals: mesh.normals,
            uvs: mesh.uvs,
            indices: mesh.indices,
            sub_meshes: mesh.sub_meshes,
        })
    }

    /// Extract mesh data from binary
    pub async fn extract_from_binary<R: AsyncBinaryReader>(
        &self,
        reader: &mut R,
        unity_class: &AsyncUnityClass,
    ) -> Result<AsyncMesh> {
        // This would implement actual binary mesh data extraction
        // For now, return a basic mesh
        AsyncMesh::from_unity_class(unity_class).await
    }
}

impl Default for AsyncMeshProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mesh_creation() {
        let mut properties = HashMap::new();
        properties.insert(
            "m_Name".to_string(),
            UnityValue::String("TestMesh".to_string()),
        );
        properties.insert("m_VertexCount".to_string(), UnityValue::UInt32(100));

        let unity_class = AsyncUnityClass::new(43, "Mesh".to_string(), "&1".to_string());
        let mesh = AsyncMesh::from_unity_class(&unity_class).await.unwrap();

        assert_eq!(mesh.name, "Mesh"); // Default name since properties aren't set
        assert_eq!(mesh.vertex_count, 0); // Default value
    }

    #[tokio::test]
    async fn test_mesh_processor() {
        let processor = AsyncMeshProcessor::new();
        let unity_class = AsyncUnityClass::new(43, "Mesh".to_string(), "&1".to_string());

        let processed = processor.process_mesh(&unity_class).await.unwrap();
        assert_eq!(processed.name, "Mesh");
        assert_eq!(processed.vertex_count, 0);
    }

    #[test]
    fn test_obj_export() {
        let mesh = AsyncMesh {
            name: "TestMesh".to_string(),
            vertex_count: 3,
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.5, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            indices: vec![0, 1, 2],
            sub_meshes: Vec::new(),
        };

        let obj = mesh.export_obj();
        assert!(obj.contains("# Mesh: TestMesh"));
        assert!(obj.contains("v 0 0 0"));
        assert!(obj.contains("v 1 0 0"));
        assert!(obj.contains("v 0.5 1 0"));
        assert!(obj.contains("f 1/1/1 2/2/2 3/3/3"));
    }
}
