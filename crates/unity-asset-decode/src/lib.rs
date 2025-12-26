//! Unity asset decode/export helpers.
//!
//! This crate intentionally depends on `unity-asset-binary` and provides optional, heavier
//! processing layers (Texture/Audio/Sprite/Mesh) behind feature flags.

pub use unity_asset_binary::{BinaryError, Result};

// Re-export core parsing modules so moved processors can keep their `crate::...` paths.
pub use unity_asset_binary::{
    asset, bundle, compression, error, file, formats, object, reader, typetree, unity_objects,
    unity_version, webfile,
};

#[cfg(feature = "texture")]
pub mod texture;

#[cfg(feature = "audio")]
pub mod audio;

#[cfg(feature = "sprite")]
pub mod sprite;

#[cfg(feature = "mesh")]
pub mod mesh;
