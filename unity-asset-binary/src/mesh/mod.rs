//! Unity Mesh processing module
//!
//! This module provides comprehensive Mesh processing capabilities,
//! organized following UnityPy and unity-rs best practices.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `types` - Core data structures (Mesh, VertexData, SubMesh, etc.)
//! - `parser` - Mesh parsing from Unity objects
//! - `processor` - High-level mesh processing and export
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::mesh::{MeshProcessor, MeshConfig};
//! use unity_asset_binary::unity_version::UnityVersion;
//! use unity_asset_binary::object::ObjectInfo;
//!
//! // Create processor with custom configuration
//! let version = UnityVersion::parse_version("2020.3.12f1")?;
//! let config = MeshConfig {
//!     extract_vertices: true,
//!     extract_indices: true,
//!     process_blend_shapes: true,
//!     decompress_meshes: true,
//!     max_vertex_count: Some(100000),
//! };
//! let processor = MeshProcessor::with_config(version, config);
//!
//! // Note: In real usage, you would create a UnityObject from parsed data
//! // For demonstration, we'll just show the processor creation
//! println!("Mesh processed successfully");
//! # Ok::<(), unity_asset_binary::error::BinaryError>(())
//! ```

pub mod parser;
pub mod processor;
pub mod types;

// Re-export main types for easy access
pub use parser::MeshParser;
pub use processor::{MeshProcessor, MeshStats};
pub use types::{
    AABB,
    BlendShape,
    BlendShapeChannel,
    // Blend shape types
    BlendShapeData,
    BlendShapeVertex,
    ChannelInfo,
    // Compression types
    CompressedMesh,
    // Core mesh types
    Mesh,
    // Configuration and results
    MeshConfig,
    MeshInfo,
    MeshResult,
    PackedFloatVector,
    PackedIntVector,
    // Streaming and info
    StreamingInfo,
    SubMesh,
    VertexData,
};

/// Main mesh processing facade
///
/// This struct provides a high-level interface for mesh processing,
/// combining parsing and processing functionality.
pub struct MeshManager {
    processor: MeshProcessor,
}

impl MeshManager {
    /// Create a new mesh manager
    pub fn new(version: crate::unity_version::UnityVersion) -> Self {
        Self {
            processor: MeshProcessor::new(version),
        }
    }

    /// Create a mesh manager with custom configuration
    pub fn with_config(version: crate::unity_version::UnityVersion, config: MeshConfig) -> Self {
        Self {
            processor: MeshProcessor::with_config(version, config),
        }
    }

    /// Process mesh from Unity object
    pub fn process_mesh(
        &self,
        object: &crate::object::UnityObject,
    ) -> crate::error::Result<MeshResult> {
        self.processor.parse_mesh(object)
    }

    /// Export mesh to OBJ format
    pub fn export_to_obj(&self, mesh: &Mesh) -> crate::error::Result<String> {
        self.processor.export_to_obj(mesh)
    }

    /// Get mesh statistics
    pub fn get_statistics(&self, meshes: &[&Mesh]) -> MeshStats {
        self.processor.get_mesh_stats(meshes)
    }

    /// Validate mesh data
    pub fn validate_mesh(&self, mesh: &Mesh) -> crate::error::Result<()> {
        self.processor.validate_mesh(mesh)
    }

    /// Get supported features
    pub fn get_supported_features(&self) -> Vec<&'static str> {
        self.processor.get_supported_features()
    }

    /// Check if a feature is supported
    pub fn is_feature_supported(&self, feature: &str) -> bool {
        self.processor.is_feature_supported(feature)
    }

    /// Get the current configuration
    pub fn config(&self) -> &MeshConfig {
        self.processor.config()
    }

    /// Set the configuration
    pub fn set_config(&mut self, config: MeshConfig) {
        self.processor.set_config(config);
    }

    /// Get the Unity version
    pub fn version(&self) -> &crate::unity_version::UnityVersion {
        self.processor.version()
    }

    /// Set the Unity version
    pub fn set_version(&mut self, version: crate::unity_version::UnityVersion) {
        self.processor.set_version(version);
    }

    /// Extract vertex data
    pub fn extract_vertices(&self, mesh: &Mesh) -> crate::error::Result<Vec<[f32; 3]>> {
        self.processor.extract_vertex_positions(mesh)
    }

    /// Extract normals
    pub fn extract_normals(&self, mesh: &Mesh) -> crate::error::Result<Vec<[f32; 3]>> {
        self.processor.extract_vertex_normals(mesh)
    }

    /// Extract UV coordinates
    pub fn extract_uvs(&self, mesh: &Mesh) -> crate::error::Result<Vec<[f32; 2]>> {
        self.processor.extract_uv_coordinates(mesh)
    }

    /// Extract triangle indices
    pub fn extract_indices(&self, mesh: &Mesh) -> crate::error::Result<Vec<u32>> {
        self.processor.extract_triangle_indices(mesh)
    }
}

impl Default for MeshManager {
    fn default() -> Self {
        Self::new(crate::unity_version::UnityVersion::default())
    }
}

/// Convenience functions for common operations
/// Create a mesh manager with default settings
pub fn create_manager(version: crate::unity_version::UnityVersion) -> MeshManager {
    MeshManager::new(version)
}

/// Create a mesh manager optimized for performance
pub fn create_performance_manager(version: crate::unity_version::UnityVersion) -> MeshManager {
    let config = MeshConfig {
        extract_vertices: false,
        extract_indices: false,
        process_blend_shapes: false,
        decompress_meshes: false,
        max_vertex_count: Some(10000),
    };
    MeshManager::with_config(version, config)
}

/// Create a mesh manager with full features
pub fn create_full_manager(version: crate::unity_version::UnityVersion) -> MeshManager {
    let config = MeshConfig {
        extract_vertices: true,
        extract_indices: true,
        process_blend_shapes: true,
        decompress_meshes: true,
        max_vertex_count: None,
    };
    MeshManager::with_config(version, config)
}

/// Parse mesh from Unity object (convenience function)
pub fn parse_mesh(
    object: &crate::object::UnityObject,
    version: &crate::unity_version::UnityVersion,
) -> crate::error::Result<Mesh> {
    let parser = MeshParser::new(version.clone());
    let result = parser.parse_from_unity_object(object)?;
    Ok(result.mesh)
}

/// Export mesh to OBJ format (convenience function)
pub fn export_mesh_to_obj(
    mesh: &Mesh,
    version: &crate::unity_version::UnityVersion,
) -> crate::error::Result<String> {
    let processor = MeshProcessor::new(version.clone());
    processor.export_to_obj(mesh)
}

/// Validate mesh data (convenience function)
pub fn validate_mesh(mesh: &Mesh) -> crate::error::Result<()> {
    let processor = MeshProcessor::default();
    processor.validate_mesh(mesh)
}

/// Get mesh vertex count
pub fn get_vertex_count(mesh: &Mesh) -> u32 {
    mesh.vertex_count()
}

/// Get mesh triangle count
pub fn get_triangle_count(mesh: &Mesh) -> u32 {
    mesh.triangle_count()
}

/// Check if mesh has blend shapes
pub fn has_blend_shapes(mesh: &Mesh) -> bool {
    mesh.has_blend_shapes()
}

/// Check if mesh is compressed
pub fn is_compressed_mesh(mesh: &Mesh) -> bool {
    mesh.is_compressed()
}

/// Check if mesh has streaming data
pub fn has_streaming_data(mesh: &Mesh) -> bool {
    mesh.has_streaming_data()
}

/// Get mesh bounds
pub fn get_mesh_bounds(mesh: &Mesh) -> &AABB {
    mesh.bounds()
}

/// Check if Unity version supports mesh feature
pub fn is_mesh_feature_supported(
    version: &crate::unity_version::UnityVersion,
    feature: &str,
) -> bool {
    match feature {
        "basic_mesh" | "sub_meshes" | "vertex_data" => true,
        "blend_shapes" | "compressed_mesh" => version.major >= 5,
        "mesh_optimization" | "streaming_info" => version.major >= 2017,
        "mesh_usage_flags" => version.major >= 2018,
        "mesh_topology" | "vertex_attributes" => version.major >= 2019,
        _ => false,
    }
}

/// Get recommended mesh configuration for Unity version
pub fn get_recommended_config(version: &crate::unity_version::UnityVersion) -> MeshConfig {
    if version.major >= 2019 {
        // Modern Unity - full features
        MeshConfig {
            extract_vertices: true,
            extract_indices: true,
            process_blend_shapes: true,
            decompress_meshes: true,
            max_vertex_count: None,
        }
    } else if version.major >= 2017 {
        // Unity 2017+ - streaming support
        MeshConfig {
            extract_vertices: true,
            extract_indices: true,
            process_blend_shapes: true,
            decompress_meshes: true,
            max_vertex_count: Some(100000),
        }
    } else if version.major >= 5 {
        // Unity 5+ - basic features
        MeshConfig {
            extract_vertices: true,
            extract_indices: true,
            process_blend_shapes: false,
            decompress_meshes: false,
            max_vertex_count: Some(50000),
        }
    } else {
        // Legacy Unity - minimal features
        MeshConfig {
            extract_vertices: false,
            extract_indices: false,
            process_blend_shapes: false,
            decompress_meshes: false,
            max_vertex_count: Some(10000),
        }
    }
}

/// Mesh processing options
#[derive(Debug, Clone)]
pub struct ProcessingOptions {
    pub parallel_processing: bool,
    pub cache_results: bool,
    pub validate_meshes: bool,
    pub generate_lods: bool,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            parallel_processing: false,
            cache_results: true,
            validate_meshes: true,
            generate_lods: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_creation() {
        let version = crate::unity_version::UnityVersion::default();
        let manager = create_manager(version);
        assert!(manager.get_supported_features().contains(&"basic_mesh"));
    }

    #[test]
    fn test_performance_manager() {
        let version = crate::unity_version::UnityVersion::default();
        let manager = create_performance_manager(version);
        assert!(!manager.config().extract_vertices);
        assert!(!manager.config().process_blend_shapes);
    }

    #[test]
    fn test_full_manager() {
        let version = crate::unity_version::UnityVersion::default();
        let manager = create_full_manager(version);
        assert!(manager.config().extract_vertices);
        assert!(manager.config().process_blend_shapes);
    }

    #[test]
    fn test_feature_support() {
        let version_2020 =
            crate::unity_version::UnityVersion::parse_version("2020.3.12f1").unwrap();
        assert!(is_mesh_feature_supported(&version_2020, "basic_mesh"));
        assert!(is_mesh_feature_supported(&version_2020, "blend_shapes"));
        assert!(is_mesh_feature_supported(
            &version_2020,
            "vertex_attributes"
        ));

        let version_2017 =
            crate::unity_version::UnityVersion::parse_version("2017.4.40f1").unwrap();
        assert!(is_mesh_feature_supported(&version_2017, "streaming_info"));
        assert!(!is_mesh_feature_supported(
            &version_2017,
            "vertex_attributes"
        ));
    }

    #[test]
    fn test_recommended_config() {
        let version_2020 =
            crate::unity_version::UnityVersion::parse_version("2020.3.12f1").unwrap();
        let config = get_recommended_config(&version_2020);
        assert!(config.extract_vertices);
        assert!(config.process_blend_shapes);
        assert!(config.decompress_meshes);

        let version_5 = crate::unity_version::UnityVersion::parse_version("5.6.7f1").unwrap();
        let config = get_recommended_config(&version_5);
        assert!(config.extract_vertices);
        assert!(!config.process_blend_shapes);
    }
}
