//! Extractors Module
//!
//! Feature-gated async extractors for Unity asset types.

#[cfg(feature = "texture")]
pub mod texture;

#[cfg(feature = "audio")]
pub mod audio;

// TODO: Implement mesh and sprite extractors
// #[cfg(feature = "mesh")]
// pub mod mesh;

// #[cfg(feature = "sprite")]
// pub mod sprite;

// Re-export main extractor types
#[cfg(feature = "texture")]
pub use texture::{ProcessedTexture, Texture2D, Texture2DProcessor, UnityTextureFormat};

#[cfg(feature = "audio")]
pub use audio::{AudioClip, AudioProcessor, ProcessedAudio, UnityAudioFormat};

// TODO: Re-export mesh and sprite types when implemented
// #[cfg(feature = "mesh")]
// pub use mesh::{AsyncMesh, AsyncMeshProcessor, ProcessedMesh};

// #[cfg(feature = "sprite")]
// pub use sprite::{AsyncSprite, AsyncSpriteProcessor, ProcessedSprite};
