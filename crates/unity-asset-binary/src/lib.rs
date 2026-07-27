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
//! This crate is intentionally **parser-only**.
//! For decoding/export helpers (Texture/Audio/Sprite), use the `unity-asset-decode` crate.
//!
//! # Example
//!
//! ```rust,no_run
//! use unity_asset_binary::bundle::BundleParser;
//! use unity_asset_core::AssetLoadBudget;
//!
//! // Load an AssetBundle file
//! let data = std::fs::read("example.bundle")?;
//! let mut budget = AssetLoadBudget::default();
//! let bundle = BundleParser::from_bytes_with_budget(data, &mut budget)?;
//!
//! // Access contained assets
//! for asset in &bundle.assets {
//!     println!("Asset with {} objects", asset.object_count());
//!     // Access objects in the asset
//!     for object in asset.objects() {
//!         println!("  Object: {} (class_id: {})", object.path_id(), object.class_id());
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

// Core modules (always available)
pub mod asset;
pub mod bundle;
mod byte_order;
pub mod compression;
pub mod data_view;
pub mod error;
pub mod file;
pub mod object;
mod random_access;
pub mod reader;
pub mod reference;
pub mod shared_bytes;
pub mod typetree;
pub mod unity_objects;
pub mod unity_version;
pub mod webfile;

pub use error::{BinaryError, BinaryObjectIdentityError, BinaryObjectReplacementError, Result};
pub use object::ObjectPayloadProvenance;
#[doc(hidden)]
pub use random_access::{ByteSegment, SegmentedBytes};

// Intentionally avoid massive top-level re-exports.
//
// Prefer importing from:
// - `unity_asset_binary::{bundle, asset, webfile, object, typetree, ...}`
// - `unity_asset_binary::file::{load_unity_file_with_budget, load_unity_file_from_memory_with_budget}`
