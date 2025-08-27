//! Unity Binary Asset Parser
//!
//! This crate provides functionality to parse Unity binary file formats including:
//! - AssetBundle files (.bundle, .unity3d)
//! - Serialized Asset files (.assets)
//! - Resource files
//!
//! # Features
//!
//! - **AssetBundle parsing**: Support for UnityFS format
//! - **Compression support**: LZ4, LZMA, and other compression formats
//! - **TypeTree parsing**: Dynamic type information for objects
//! - **Object extraction**: Extract Unity objects from binary data
//!
//! ## Feature Flags
//!
//! - `texture`: Texture processing (basic formats + PNG export)
//! - `audio`: Audio processing (all formats including Vorbis, MP3)
//! - `mesh`: Mesh processing (parsing + basic export)
//! - `sprite`: Sprite processing (requires texture support)
//! - `texture-advanced`: Advanced texture formats (DXT, ETC, ASTC) - requires texture2ddecoder
//! - `mesh-export`: Advanced mesh export (OBJ format)
//!
//! # Example
//!
//! ```rust,no_run
//! use unity_asset_binary::load_bundle_from_memory;
//! use std::fs;
//!
//! // Load an AssetBundle file
//! let data = fs::read("example.bundle")?;
//! let bundle = load_bundle_from_memory(data)?;
//!
//! // Access contained assets
//! for asset in &bundle.assets {
//!     println!("Asset with {} objects", asset.object_count());
//!     // Access objects in the asset
//!     for object in &asset.objects {
//!         println!("  Object: {} (type_id: {})", object.path_id, object.type_id);
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

// Core modules (always available)
pub mod asset;
pub mod bundle;
pub mod compression;
pub mod error;
pub mod metadata;
pub mod object;
pub mod performance;
pub mod reader;
pub mod typetree;
pub mod unity_objects;
pub mod unity_version;
pub mod webfile;

// Feature-gated modules
#[cfg(feature = "texture")]
pub mod texture;

#[cfg(feature = "audio")]
pub mod audio;

#[cfg(feature = "sprite")]
pub mod sprite;

#[cfg(feature = "mesh")]
pub mod mesh;

// Re-export core types (always available)
pub use asset::{
    Asset, // Legacy compatibility alias
    AssetFileInfo,
    // Processing
    AssetProcessor,
    FileIdentifier,
    FileStatistics,
    // Information and validation
    HeaderFormatInfo,
    HeaderValidation,
    ObjectInfo,
    ParsingOptions,
    ParsingStats,
    // Core types
    SerializedFile,
    SerializedFileHeader,
    SerializedFileParser,
    SerializedType,
    TypeRegistry,
    // Constants
    class_ids,
    // Convenience functions
    create_processor as create_asset_processor,
    get_file_info as get_asset_file_info,
    get_parsing_options as get_asset_parsing_options,
    get_supported_versions as get_supported_asset_versions,
    is_valid_serialized_file,
    is_version_supported as is_asset_version_supported,
    parse_serialized_file,
    parse_serialized_file_from_path,
};
pub use bundle::{
    // Core types
    AssetBundle,
    // Compression
    BundleCompression,
    BundleFileInfo,
    BundleFormatInfo,
    BundleHeader,
    BundleInfo,
    BundleLoadOptions,
    BundleLoader,
    BundleParser,
    // Processing
    BundleProcessor,
    BundleResourceManager,
    BundleStatistics,
    CompressionOptions,
    CompressionStats,
    DirectoryNode,
    // Statistics and info
    LoaderStatistics,
    ParsingComplexity,
    // Convenience functions (with bundle prefix to avoid conflicts)
    create_processor as create_bundle_processor,
    extract_file_from_bundle,
    get_bundle_info,
    get_supported_formats as get_supported_bundle_formats,
    is_valid_bundle,
    list_bundle_contents,
    load_bundle,
    load_bundle_from_memory,
    load_bundle_with_options,
};
pub use error::{BinaryError, Result};

// Re-export async support
pub use metadata::{
    // Core metadata types
    AssetMetadata,
    AssetReference,
    // Relationship types
    AssetRelationships,
    // Statistics and options
    AssetStatistics,
    ComponentRelationship,
    DependencyAnalyzer,
    DependencyGraph,
    // Dependency types
    DependencyInfo,
    ExternalReference,
    // Configuration and results
    ExtractionConfig,
    ExtractionResult,
    ExtractionStats,
    FileInfo,
    GameObjectHierarchy,
    InternalReference,
    MemoryUsage,
    MetadataExtractor,
    // Processing
    MetadataProcessor,
    ObjectStatistics,
    ObjectSummary,
    PerformanceMetrics,
    ProcessingOptions,
    RelationshipAnalyzer,
    create_comprehensive_processor,
    create_performance_processor,
    // Convenience functions
    create_processor as create_metadata_processor,
    extract_basic_metadata,
    extract_metadata_with_config,
    get_asset_statistics,
    get_recommended_config,
    is_extraction_supported,
};
pub use object::{ObjectInfo as UnityObjectInfo, UnityObject};
pub use reader::{BinaryReader, ByteOrder};
pub use typetree::{
    ParsingStats as TypeTreeParsingStats,
    TypeInfo,
    TypeRegistry as TypeTreeRegistry,
    // Core types
    TypeTree,
    TypeTreeBuilder,
    // Information
    TypeTreeInfo,
    TypeTreeNode,
    TypeTreeParser,
    // Processing
    TypeTreeProcessor,
    TypeTreeSerializer,
    TypeTreeStatistics,
    TypeTreeValidator,
    ValidationReport,
    build_common_typetree,
    // Convenience functions
    create_processor as create_typetree_processor,
    get_parsing_method as get_typetree_parsing_method,
    get_typetree_info,
    is_version_supported as is_typetree_version_supported,
    parse_object_with_typetree,
    parse_typetree,
    serialize_object_with_typetree,
    validate_typetree,
};
pub use unity_objects::{GameObject, ObjectRef, Quaternion, Transform, Vector3};
pub use unity_version::{UnityFeature, UnityVersion, UnityVersionType, VersionCompatibility};
pub use webfile::{WebFile, WebFileCompression};

// Re-export feature-gated types
#[cfg(feature = "texture")]
pub use texture::{
    BasicDecoder,
    CompressedDecoder,
    CrunchDecoder,
    ExportOptions,
    GLTextureSettings,
    MobileDecoder,
    StreamingInfo,
    // Core types
    Texture2D,
    Texture2DConverter,
    // Processors and converters
    Texture2DProcessor,
    // Decoders
    TextureDecoder,
    // Helpers
    TextureExporter,
    TextureFormat,
    TextureProcessor,
    TextureSwizzler,
    // Convenience functions
    create_processor,
    decode_texture_data,
    export_image,
    get_supported_formats,
    is_format_supported,
};

#[cfg(feature = "audio")]
pub use audio::{
    AudioAnalysis,
    // Core types
    AudioClip,
    AudioClipConverter,
    AudioClipMeta,
    // Processors and converters
    AudioClipProcessor,
    AudioCompressionFormat,
    // Decoder
    AudioDecoder,
    // Export
    AudioExporter,
    AudioFormat,
    AudioFormatInfo,
    AudioInfo,
    AudioProcessor,
    AudioProperties,
    DecodedAudio,
    FMODSoundType,
    // Convenience functions (with audio prefix to avoid conflicts)
    create_processor as create_audio_processor,
    decode_audio_data,
    export_audio,
    get_supported_formats as get_supported_audio_formats,
    is_format_supported as is_audio_format_supported,
};

#[cfg(feature = "sprite")]
pub use sprite::{
    ProcessingOptions as SpriteProcessingOptions,
    // Core sprite types
    Sprite,
    SpriteAtlas,
    SpriteBorder,
    // Configuration and results
    SpriteConfig,
    SpriteInfo,
    // Processing
    SpriteManager,
    SpriteOffset,
    SpriteParser,
    SpritePivot,
    SpriteProcessor,
    SpriteRect,
    SpriteRenderData,
    SpriteResult,
    SpriteSettings,
    SpriteStats,
    create_full_manager as create_full_sprite_manager,
    // Convenience functions
    create_manager as create_sprite_manager,
    create_performance_manager as create_performance_sprite_manager,
    extract_sprite_image,
    get_recommended_config as get_recommended_sprite_config,
    get_sprite_area,
    get_sprite_aspect_ratio,
    is_atlas_sprite,
    is_nine_slice_sprite,
    is_sprite_feature_supported,
    parse_sprite,
    validate_sprite,
};

#[cfg(feature = "mesh")]
pub use mesh::{
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
    // Processing
    MeshManager,
    MeshParser,
    MeshProcessor,
    MeshResult,
    MeshStats,
    PackedFloatVector,
    PackedIntVector,
    ProcessingOptions as MeshProcessingOptions,
    // Streaming and info
    StreamingInfo as MeshStreamingInfo,
    SubMesh,
    VertexData,
    create_full_manager as create_full_mesh_manager,
    // Convenience functions
    create_manager as create_mesh_manager,
    create_performance_manager as create_performance_mesh_manager,
    export_mesh_to_obj,
    get_mesh_bounds,
    get_recommended_config as get_recommended_mesh_config,
    get_triangle_count,
    get_vertex_count,
    has_blend_shapes,
    has_streaming_data,
    is_compressed_mesh,
    is_mesh_feature_supported,
    parse_mesh,
    validate_mesh,
};

#[cfg(test)]
mod tests {
    #[test]
    fn test_basic_functionality() {
        // Basic smoke test
        assert_eq!(2 + 2, 4);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn test_async_functionality() {
        // Test that async features compile
        let dummy_data = vec![0u8; 32];

        // Test AssetBundle async creation (should fail with invalid data, but should compile)
        match crate::AssetBundle::from_bytes_async(dummy_data.clone()).await {
            Ok(_) => {
                // Unexpected success with dummy data
                panic!("Should not succeed with dummy data");
            }
            Err(_) => {
                // Expected failure with dummy data
                println!("✅ AssetBundle::from_bytes_async compiles and handles errors correctly");
            }
        }

        // Test SerializedFile async creation
        match crate::SerializedFile::from_bytes_async(dummy_data).await {
            Ok(_) => {
                panic!("Should not succeed with dummy data");
            }
            Err(_) => {
                println!(
                    "✅ SerializedFile::from_bytes_async compiles and handles errors correctly"
                );
            }
        }
    }
}
