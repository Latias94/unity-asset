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
pub use asset::{Asset, SerializedFile};
pub use bundle::AssetBundle;
pub use error::{BinaryError, Result};

// Re-export async support
pub use metadata::{AssetMetadata, DependencyInfo, MetadataExtractor, ObjectStatistics};
pub use object::{ObjectInfo, UnityObject};
pub use reader::{BinaryReader, ByteOrder};
pub use typetree::{TypeTree, TypeTreeNode};
pub use unity_objects::{GameObject, ObjectRef, Quaternion, Transform, Vector3};
pub use unity_version::{UnityFeature, UnityVersion, UnityVersionType, VersionCompatibility};
pub use webfile::{WebFile, WebFileCompression};

// Re-export feature-gated types
#[cfg(feature = "texture")]
pub use texture::{Texture2D, Texture2DProcessor, TextureFormat};

#[cfg(feature = "audio")]
pub use audio::{
    AudioClip, AudioClipMeta, AudioClipProcessor, AudioCompressionFormat, AudioFormatInfo,
    AudioInfo,
};

#[cfg(feature = "sprite")]
pub use sprite::{Sprite, SpriteInfo, SpriteProcessor};

#[cfg(feature = "mesh")]
pub use mesh::{Mesh, MeshInfo, MeshProcessor};

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
