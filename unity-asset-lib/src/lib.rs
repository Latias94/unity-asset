//! Unity Asset Parser
//!
//! A comprehensive Rust library for parsing Unity asset files, supporting both YAML and binary formats.
//!
//! This crate provides high-performance, memory-safe parsing of Unity files
//! while aiming for best-effort compatibility with Unity's formats (correctness and coverage are ongoing work).
//!
//! # Features
//!
//! - **YAML Processing**: Complete Unity YAML format support with multi-document parsing
//! - **Binary Assets**: AssetBundle and SerializedFile parsing with compression support
//! - **Async Support**: Optional async/await API for concurrent processing (enable with `async` feature)
//! - **Type Safety**: Rust's type system prevents common parsing vulnerabilities
//! - **Performance**: Designed for reasonable performance; some workflows may be eager by default
//!
//! # Examples
//!
//! ## Basic YAML Processing
//!
//! ```rust,no_run
//! use unity_asset::{YamlDocument, UnityDocument};
//!
//! // Load a Unity YAML file
//! let doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;
//!
//! // Access and filter objects
//! let settings = doc.get(Some("PlayerSettings"), None)?;
//! println!("Product name: {:?}", settings.get("productName"));
//!
//! # Ok::<(), unity_asset::UnityAssetError>(())
//! ```
//!
//! ## Binary Asset Processing
//!
//! ```rust,no_run
//! use unity_asset::load_bundle_from_memory;
//!
//! // Load and parse AssetBundle
//! let data = std::fs::read("game.bundle")?;
//! let bundle = load_bundle_from_memory(data)?;
//!
//! // Process assets
//! for asset in &bundle.assets {
//!     println!("Found asset with {} objects", asset.object_count());
//! }
//!
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Async Processing (requires `async` feature)
//!
//! ```rust,no_run
//! # #[cfg(feature = "async")]
//! # {
//! use unity_asset::{YamlDocument, AsyncUnityDocument};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Load file asynchronously
//!     let doc = YamlDocument::load_yaml_async("ProjectSettings.asset", false).await?;
//!
//!     // Same API as sync version
//!     let settings = doc.get(Some("PlayerSettings"), None)?;
//!     println!("Product name: {:?}", settings.get("productName"));
//!
//!     Ok(())
//! }
//! # }
//! ```

// Re-export from core crate
pub use unity_asset_core::{
    DocumentFormat, Result, UnityAssetError, UnityClass, UnityClassRegistry, UnityDocument,
    UnityValue, constants::*,
};

pub use unity_asset_core::get_class_name;

// Re-export from YAML crate
pub use unity_asset_yaml::YamlDocument;

// Re-export from binary crate
pub use unity_asset_binary::asset::SerializedFile;
pub use unity_asset_binary::bundle::{
    AssetBundle, load_bundle, load_bundle_from_memory, load_bundle_with_options,
};

// Re-export async traits when async feature is enabled
#[cfg(feature = "async")]
pub use unity_asset_core::document::AsyncUnityDocument;

/// Environment for managing multiple Unity assets
pub mod environment;
