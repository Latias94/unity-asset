//! Unity Asset Binary V2
//!
//! Fully async binary processing for Unity assets with streaming support,
//! concurrent operations, and efficient memory management.
//!
//! # Key Features
//!
//! - **Fully Async**: All I/O and processing operations are async
//! - **Stream Processing**: Memory-efficient processing of large binary files
//! - **Concurrent Operations**: Parallel asset loading and processing
//! - **Bundle Support**: Full AssetBundle format support (UnityFS, etc.)
//! - **Compression**: LZ4, LZMA, Brotli decompression with async support
//! - **Asset Extraction**: AudioClip, Texture2D, Mesh, Sprite processing
//!
//! # Architecture
//!
//! The v2 implementation is designed for modularity and async efficiency:
//!
//! - `async_bundle.rs` - Async AssetBundle parsing and processing
//! - `async_asset.rs` - Async SerializedFile and Asset handling  
//! - `async_compression.rs` - Async compression/decompression
//! - `stream_reader.rs` - Async binary reading with backpressure
//! - `object_processor.rs` - Async Unity object processing
//! - `extractors/` - Feature-gated async extractors (audio, texture, etc.)
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use unity_asset_binary_v2::{AsyncAssetBundle, AsyncBundleProcessor};
//! use futures::StreamExt;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Load bundle asynchronously
//!     let bundle = AsyncAssetBundle::load_from_path("example.bundle").await?;
//!     
//!     // Process assets concurrently
//!     let mut asset_stream = bundle.assets_stream();
//!     while let Some(asset_result) = asset_stream.next().await {
//!         let asset = asset_result?;
//!         println!("Processing asset: {}", asset.name());
//!         
//!         // Extract objects from asset
//!         let mut object_stream = asset.objects_stream();
//!         while let Some(object_result) = object_stream.next().await {
//!             let object = object_result?;
//!             println!("  Object: {} ({})",
//!                 object.name().unwrap_or("Unknown"),
//!                 object.class_name()
//!             );
//!         }
//!     }
//!     
//!     Ok(())
//! }
//! ```

// Re-export core types
pub use unity_asset_core_v2::*;

// Core async binary modules
pub mod async_asset;
pub mod async_bundle;
pub mod async_compression;
pub mod binary_types;
pub mod object_processor;
pub mod stream_reader;

// Feature-gated extractors
pub mod extractors;

pub use extractors::texture::*;

pub use extractors::audio::*;

// TODO: Re-export mesh and sprite extractors when implemented
// #[cfg(feature = "mesh")]
// pub use extractors::mesh::*;

// #[cfg(feature = "sprite")]
// pub use extractors::sprite::*;

// Re-export main types
pub use async_asset::{AssetConfig, AsyncAsset, AsyncSerializedFile};
pub use async_bundle::{AsyncAssetBundle, AsyncBundleProcessor, BundleConfig};
pub use async_compression::{AsyncDecompressor, CompressionConfig};
pub use binary_types::*;
pub use object_processor::{AsyncObjectProcessor, ObjectConfig};
pub use stream_reader::ReaderConfig;

// Re-export main extractors for direct access
pub use extractors::*;

/// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Initialize async binary processor with default settings
pub fn init_async_binary() -> AsyncBundleProcessor {
    AsyncBundleProcessor::new()
}

/// Create async binary processor with custom configuration  
pub fn init_async_binary_with_config(config: BundleConfig) -> AsyncBundleProcessor {
    AsyncBundleProcessor::with_config(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_basic_binary_functionality() {
        let processor = init_async_binary();
        // Basic smoke test
        assert!(processor.max_concurrent_bundles() > 0);
    }
}
