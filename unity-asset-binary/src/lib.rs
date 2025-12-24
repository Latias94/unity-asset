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
//! use unity_asset_binary::bundle::load_bundle_from_memory;
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
pub mod file;
pub mod formats;
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

pub use error::{BinaryError, Result};

// Intentionally avoid massive top-level re-exports.
//
// Prefer importing from:
// - `unity_asset_binary::formats::{bundle, serialized, web}`
// - `unity_asset_binary::{bundle, asset, webfile, object, typetree, ...}`
// - `unity_asset_binary::file::{load_unity_file, load_unity_file_from_memory}`

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
        let dummy_data = [0u8; 32];

        // Test basic async functionality - for now just verify the feature compiles
        // TODO: Implement actual async methods when needed
        let _result = tokio::task::yield_now().await;

        println!("✅ Async feature compiles correctly");

        // Note: AssetBundle::from_bytes_async and SerializedFile::from_bytes_async
        // are not yet implemented. They would be added when async support is needed.
        assert!(!dummy_data.is_empty());
    }
}
