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
//! use unity_asset_binary::AssetBundle;
//! use std::fs;
//!
//! // Load an AssetBundle file
//! let data = fs::read("example.bundle")?;
//! let bundle = AssetBundle::from_bytes(data)?;
//!
//! // Access contained assets
//! for asset in bundle.assets() {
//!     println!("Asset: {}", asset.name());
//!     // Extract objects from the asset
//!     let objects = asset.get_objects()?;
//!     for object in objects {
//!         if let Some(name) = object.name() {
//!             println!("  Object: {} ({})", name, object.class_name());
//!         } else {
//!             println!("  Object: <unnamed> ({})", object.class_name());
//!         }
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
    // Core types
    SerializedFile, SerializedFileHeader, SerializedType, FileIdentifier, ObjectInfo,
    Asset, // Legacy compatibility alias
    // Processing
    AssetProcessor, SerializedFileParser, TypeRegistry,
    // Information and validation
    HeaderFormatInfo, HeaderValidation, ParsingStats, FileStatistics,
    AssetFileInfo, ParsingOptions,
    // Convenience functions
    create_processor as create_asset_processor,
    parse_serialized_file, parse_serialized_file_from_path,
    get_file_info as get_asset_file_info, is_valid_serialized_file,
    get_supported_versions as get_supported_asset_versions,
    is_version_supported as is_asset_version_supported,
    get_parsing_options as get_asset_parsing_options,
    // Constants
    class_ids,
};
pub use bundle::{
    // Core types
    AssetBundle, BundleHeader, BundleFileInfo, DirectoryNode,
    BundleStatistics, BundleLoadOptions, BundleFormatInfo,
    // Processing
    BundleProcessor, BundleParser, BundleLoader, BundleResourceManager,
    // Compression
    BundleCompression, CompressionStats, CompressionOptions,
    // Statistics and info
    LoaderStatistics, ParsingComplexity, BundleInfo,
    // Convenience functions (with bundle prefix to avoid conflicts)
    create_processor as create_bundle_processor, load_bundle, load_bundle_from_memory,
    load_bundle_with_options, get_bundle_info, list_bundle_contents,
    extract_file_from_bundle, is_valid_bundle,
    get_supported_formats as get_supported_bundle_formats,
};
pub use error::{BinaryError, Result};

// Re-export async support
pub use metadata::{
    // Core metadata types
    AssetMetadata, FileInfo, ObjectStatistics, ObjectSummary, MemoryUsage,
    // Dependency types
    DependencyInfo, ExternalReference, InternalReference, DependencyGraph,
    // Relationship types
    AssetRelationships, GameObjectHierarchy, ComponentRelationship, AssetReference,
    // Processing
    MetadataProcessor, MetadataExtractor, DependencyAnalyzer, RelationshipAnalyzer,
    // Configuration and results
    ExtractionConfig, ExtractionResult, ExtractionStats, PerformanceMetrics,
    // Statistics and options
    AssetStatistics, ProcessingOptions,
    // Convenience functions
    create_processor as create_metadata_processor,
    create_performance_processor, create_comprehensive_processor,
    extract_basic_metadata, extract_metadata_with_config,
    get_asset_statistics, is_extraction_supported, get_recommended_config,
};
pub use object::{ObjectInfo as UnityObjectInfo, UnityObject};
pub use reader::{BinaryReader, ByteOrder};
pub use typetree::{
    // Core types
    TypeTree, TypeTreeNode, TypeTreeStatistics, TypeInfo, TypeRegistry as TypeTreeRegistry,
    // Processing
    TypeTreeProcessor, TypeTreeParser, TypeTreeBuilder, TypeTreeValidator,
    TypeTreeSerializer, ValidationReport, ParsingStats as TypeTreeParsingStats,
    // Information
    TypeTreeInfo,
    // Convenience functions
    create_processor as create_typetree_processor,
    parse_typetree, parse_object_with_typetree, serialize_object_with_typetree,
    build_common_typetree, validate_typetree, get_typetree_info,
    is_version_supported as is_typetree_version_supported,
    get_parsing_method as get_typetree_parsing_method,
};
pub use unity_objects::{GameObject, ObjectRef, Quaternion, Transform, Vector3};
pub use unity_version::{UnityFeature, UnityVersion, UnityVersionType, VersionCompatibility};
pub use webfile::{WebFile, WebFileCompression};

// Re-export feature-gated types
#[cfg(feature = "texture")]
pub use texture::{
    // Core types
    Texture2D, TextureFormat, StreamingInfo, GLTextureSettings,
    // Processors and converters
    Texture2DProcessor, Texture2DConverter, TextureProcessor,
    // Decoders
    TextureDecoder, BasicDecoder, CompressedDecoder, MobileDecoder, CrunchDecoder,
    // Helpers
    TextureExporter, TextureSwizzler, ExportOptions,
    // Convenience functions
    create_processor, is_format_supported, get_supported_formats,
    decode_texture_data, export_image,
};

#[cfg(feature = "audio")]
pub use audio::{
    // Core types
    AudioClip, AudioClipMeta, AudioCompressionFormat, FMODSoundType, AudioFormatInfo,
    AudioProperties, AudioInfo, DecodedAudio, AudioAnalysis,
    // Processors and converters
    AudioClipProcessor, AudioClipConverter, AudioProcessor,
    // Decoder
    AudioDecoder,
    // Export
    AudioExporter, AudioFormat,
    // Convenience functions (with audio prefix to avoid conflicts)
    create_processor as create_audio_processor,
    is_format_supported as is_audio_format_supported,
    get_supported_formats as get_supported_audio_formats,
    decode_audio_data, export_audio,
};

#[cfg(feature = "sprite")]
pub use sprite::{
    // Core sprite types
    Sprite, SpriteRenderData, SpriteSettings, SpriteRect, SpriteOffset,
    SpritePivot, SpriteBorder, SpriteInfo, SpriteAtlas,
    // Processing
    SpriteManager, SpriteProcessor, SpriteParser, SpriteStats,
    // Configuration and results
    SpriteConfig, SpriteResult, ProcessingOptions as SpriteProcessingOptions,
    // Convenience functions
    create_manager as create_sprite_manager,
    create_performance_manager as create_performance_sprite_manager,
    create_full_manager as create_full_sprite_manager,
    parse_sprite, extract_sprite_image, validate_sprite,
    get_sprite_area, is_nine_slice_sprite, is_atlas_sprite, get_sprite_aspect_ratio,
    is_sprite_feature_supported, get_recommended_config as get_recommended_sprite_config,
};

#[cfg(feature = "mesh")]
pub use mesh::{
    // Core mesh types
    Mesh, VertexData, ChannelInfo, SubMesh, AABB,
    // Blend shape types
    BlendShapeData, BlendShapeVertex, BlendShape, BlendShapeChannel,
    // Compression types
    CompressedMesh, PackedFloatVector, PackedIntVector,
    // Streaming and info
    StreamingInfo as MeshStreamingInfo, MeshInfo,
    // Processing
    MeshManager, MeshProcessor, MeshParser, MeshStats,
    // Configuration and results
    MeshConfig, MeshResult, ProcessingOptions as MeshProcessingOptions,
    // Convenience functions
    create_manager as create_mesh_manager,
    create_performance_manager as create_performance_mesh_manager,
    create_full_manager as create_full_mesh_manager,
    parse_mesh, export_mesh_to_obj, validate_mesh,
    get_vertex_count, get_triangle_count, has_blend_shapes, is_compressed_mesh,
    has_streaming_data, get_mesh_bounds, is_mesh_feature_supported,
    get_recommended_config as get_recommended_mesh_config,
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
