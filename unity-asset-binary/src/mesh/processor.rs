//! Mesh processing implementation
//!
//! This module provides high-level mesh processing functionality including
//! mesh export and optimization.

use super::parser::MeshParser;
use super::types::*;
use crate::error::Result;
use crate::object::UnityObject;
use crate::unity_version::UnityVersion;

/// Mesh processor
///
/// This struct provides high-level methods for processing Unity Mesh objects,
/// including parsing, validation, and export functionality.
pub struct MeshProcessor {
    parser: MeshParser,
    config: MeshConfig,
}

impl MeshProcessor {
    /// Create a new Mesh processor
    pub fn new(version: UnityVersion) -> Self {
        Self {
            parser: MeshParser::new(version),
            config: MeshConfig::default(),
        }
    }

    /// Create a Mesh processor with custom configuration
    pub fn with_config(version: UnityVersion, config: MeshConfig) -> Self {
        Self {
            parser: MeshParser::new(version),
            config,
        }
    }

    /// Parse Mesh from Unity object
    pub fn parse_mesh(&self, object: &UnityObject) -> Result<MeshResult> {
        let mut result = self.parser.parse_from_unity_object(object)?;

        // Apply configuration-based processing
        if let Some(max_vertices) = self.config.max_vertex_count {
            if result.mesh.vertex_count() > max_vertices {
                result.add_warning(format!(
                    "Mesh has {} vertices, exceeding limit of {}",
                    result.mesh.vertex_count(),
                    max_vertices
                ));
            }
        }

        // Validate mesh if needed
        if let Err(e) = self.validate_mesh(&result.mesh) {
            result.add_warning(format!("Mesh validation failed: {}", e));
        }

        Ok(result)
    }

    /// Validate mesh data
    pub fn validate_mesh(&self, mesh: &Mesh) -> Result<()> {
        // Check basic validity
        if mesh.name.is_empty() {
            return Err(crate::error::BinaryError::invalid_data("Mesh has no name"));
        }

        if mesh.vertex_data.vertex_count == 0 {
            return Err(crate::error::BinaryError::invalid_data(
                "Mesh has no vertices",
            ));
        }

        // Check submeshes
        for (i, submesh) in mesh.sub_meshes.iter().enumerate() {
            if !submesh.is_valid() {
                return Err(crate::error::BinaryError::invalid_data(format!(
                    "SubMesh {} is invalid",
                    i
                )));
            }
        }

        // Check vertex count limits
        if let Some(max_vertices) = self.config.max_vertex_count {
            if mesh.vertex_count() > max_vertices {
                return Err(crate::error::BinaryError::invalid_data(format!(
                    "Mesh vertex count {} exceeds limit {}",
                    mesh.vertex_count(),
                    max_vertices
                )));
            }
        }

        Ok(())
    }

    /// Export mesh to OBJ format
    pub fn export_to_obj(&self, mesh: &Mesh) -> Result<String> {
        let mut obj_data = String::new();

        // Header
        obj_data.push_str("# Exported from Unity Asset Parser\n");
        obj_data.push_str(&format!("# Mesh: {}\n", mesh.name));
        obj_data.push_str(&format!("# Vertices: {}\n", mesh.vertex_count()));
        obj_data.push_str(&format!("# SubMeshes: {}\n", mesh.sub_meshes.len()));
        obj_data.push_str("\n");

        // Export vertices (placeholder - would need actual vertex data parsing)
        obj_data.push_str("# Vertices\n");
        for _i in 0..mesh.vertex_count() {
            obj_data.push_str(&format!("v 0.0 0.0 0.0\n"));
        }

        // Export normals (placeholder)
        obj_data.push_str("\n# Normals\n");
        for _i in 0..mesh.vertex_count() {
            obj_data.push_str(&format!("vn 0.0 1.0 0.0\n"));
        }

        // Export UV coordinates (placeholder)
        obj_data.push_str("\n# UV Coordinates\n");
        for _i in 0..mesh.vertex_count() {
            obj_data.push_str(&format!("vt 0.0 0.0\n"));
        }

        // Export faces (placeholder)
        obj_data.push_str("\n# Faces\n");
        if !mesh.sub_meshes.is_empty() {
            for (i, sub_mesh) in mesh.sub_meshes.iter().enumerate() {
                obj_data.push_str(&format!(
                    "# SubMesh {}: {} triangles\n",
                    i, sub_mesh.triangle_count
                ));

                // Would need to parse actual index data here
                for j in 0..sub_mesh.triangle_count {
                    let base = j * 3 + 1; // OBJ indices are 1-based
                    obj_data.push_str(&format!(
                        "f {}/{}/{} {}/{}/{} {}/{}/{}\n",
                        base,
                        base,
                        base,
                        base + 1,
                        base + 1,
                        base + 1,
                        base + 2,
                        base + 2,
                        base + 2
                    ));
                }
            }
        }

        Ok(obj_data)
    }

    /// Get mesh statistics
    pub fn get_mesh_stats(&self, meshes: &[&Mesh]) -> MeshStats {
        let mut stats = MeshStats::default();

        stats.total_meshes = meshes.len();

        for mesh in meshes {
            stats.total_vertices += mesh.vertex_count();
            stats.total_triangles += mesh.triangle_count();
            stats.total_submeshes += mesh.sub_meshes.len() as u32;

            if mesh.has_blend_shapes() {
                stats.meshes_with_blend_shapes += 1;
            }

            if mesh.is_compressed() {
                stats.compressed_meshes += 1;
            }

            if mesh.has_streaming_data() {
                stats.streaming_meshes += 1;
            }

            // Track complexity
            let vertex_count = mesh.vertex_count();
            if vertex_count < 1000 {
                stats.low_poly_meshes += 1;
            } else if vertex_count < 10000 {
                stats.medium_poly_meshes += 1;
            } else {
                stats.high_poly_meshes += 1;
            }
        }

        if !meshes.is_empty() {
            stats.average_vertices = stats.total_vertices as f32 / meshes.len() as f32;
            stats.average_triangles = stats.total_triangles as f32 / meshes.len() as f32;
        }

        stats
    }

    /// Get supported mesh features for this Unity version
    pub fn get_supported_features(&self) -> Vec<&'static str> {
        let version = self.parser.version();
        let mut features = vec!["basic_mesh", "sub_meshes", "vertex_data"];

        if version.major >= 5 {
            features.push("blend_shapes");
            features.push("compressed_mesh");
        }

        if version.major >= 2017 {
            features.push("mesh_optimization");
            features.push("streaming_info");
        }

        if version.major >= 2018 {
            features.push("mesh_usage_flags");
        }

        if version.major >= 2019 {
            features.push("mesh_topology");
            features.push("vertex_attributes");
        }

        features
    }

    /// Check if a feature is supported
    pub fn is_feature_supported(&self, feature: &str) -> bool {
        self.get_supported_features().contains(&feature)
    }

    /// Get the current configuration
    pub fn config(&self) -> &MeshConfig {
        &self.config
    }

    /// Set the configuration
    pub fn set_config(&mut self, config: MeshConfig) {
        self.config = config;
    }

    /// Get the Unity version
    pub fn version(&self) -> &UnityVersion {
        self.parser.version()
    }

    /// Set the Unity version
    pub fn set_version(&mut self, version: UnityVersion) {
        self.parser.set_version(version);
    }

    /// Extract vertex positions (if available)
    pub fn extract_vertex_positions(&self, _mesh: &Mesh) -> Result<Vec<[f32; 3]>> {
        // This would require parsing the actual vertex data
        // For now, return empty vector
        Ok(Vec::new())
    }

    /// Extract vertex normals (if available)
    pub fn extract_vertex_normals(&self, _mesh: &Mesh) -> Result<Vec<[f32; 3]>> {
        // This would require parsing the actual vertex data
        // For now, return empty vector
        Ok(Vec::new())
    }

    /// Extract UV coordinates (if available)
    pub fn extract_uv_coordinates(&self, _mesh: &Mesh) -> Result<Vec<[f32; 2]>> {
        // This would require parsing the actual vertex data
        // For now, return empty vector
        Ok(Vec::new())
    }

    /// Extract triangle indices
    pub fn extract_triangle_indices(&self, _mesh: &Mesh) -> Result<Vec<u32>> {
        // This would require parsing the index buffer
        // For now, return empty vector
        Ok(Vec::new())
    }
}

impl Default for MeshProcessor {
    fn default() -> Self {
        Self::new(UnityVersion::default())
    }
}

/// Mesh processing statistics
#[derive(Debug, Clone, Default)]
pub struct MeshStats {
    pub total_meshes: usize,
    pub total_vertices: u32,
    pub total_triangles: u32,
    pub total_submeshes: u32,
    pub average_vertices: f32,
    pub average_triangles: f32,
    pub meshes_with_blend_shapes: usize,
    pub compressed_meshes: usize,
    pub streaming_meshes: usize,
    pub low_poly_meshes: usize,    // < 1K vertices
    pub medium_poly_meshes: usize, // 1K-10K vertices
    pub high_poly_meshes: usize,   // > 10K vertices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_creation() {
        let version = UnityVersion::default();
        let processor = MeshProcessor::new(version);
        assert_eq!(processor.version(), &UnityVersion::default());
    }

    #[test]
    fn test_supported_features() {
        let version = UnityVersion::parse_version("2020.3.12f1").unwrap();
        let processor = MeshProcessor::new(version);

        let features = processor.get_supported_features();
        assert!(features.contains(&"basic_mesh"));
        assert!(features.contains(&"blend_shapes"));
        assert!(features.contains(&"mesh_optimization"));
        assert!(features.contains(&"mesh_usage_flags"));
        assert!(processor.is_feature_supported("vertex_attributes"));
    }

    #[test]
    fn test_mesh_validation() {
        let processor = MeshProcessor::default();
        let mut mesh = Mesh::default();

        // Invalid mesh (no name, no vertices)
        assert!(processor.validate_mesh(&mesh).is_err());

        // Valid mesh
        mesh.name = "TestMesh".to_string();
        mesh.vertex_data.vertex_count = 100;
        assert!(processor.validate_mesh(&mesh).is_ok());
    }

    #[test]
    fn test_mesh_stats() {
        let processor = MeshProcessor::default();
        let mut mesh1 = Mesh::default();
        mesh1.vertex_data.vertex_count = 1000;
        mesh1.sub_meshes.push(SubMesh {
            triangle_count: 500,
            ..Default::default()
        });

        let mut mesh2 = Mesh::default();
        mesh2.vertex_data.vertex_count = 2000;
        mesh2.sub_meshes.push(SubMesh {
            triangle_count: 1000,
            ..Default::default()
        });

        let meshes = vec![&mesh1, &mesh2];
        let stats = processor.get_mesh_stats(&meshes);

        assert_eq!(stats.total_meshes, 2);
        assert_eq!(stats.total_vertices, 3000);
        assert_eq!(stats.total_triangles, 1500);
        assert_eq!(stats.average_vertices, 1500.0);
    }
}
