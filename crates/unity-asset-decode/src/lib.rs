//! Unity asset decode/export helpers.
//!
//! This crate intentionally depends on `unity-asset-binary` and provides optional, heavier
//! processing layers (Texture/Audio/Sprite) behind feature flags.

pub mod descriptor;
pub mod media;

#[cfg(feature = "texture")]
pub mod texture;

#[cfg(feature = "audio")]
pub mod audio;

#[cfg(feature = "sprite")]
pub mod sprite;
