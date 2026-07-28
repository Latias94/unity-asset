//! Unity Sprite parsing, rendering, and caller-owned encoding.
//!
//! `SpriteParser` reads Sprite metadata. `SpriteProcessor` adds explicit
//! texture decoding, atlas processing, and PNG writing without taking
//! ownership of filesystem paths.

pub mod parser;
pub mod processor;
pub mod types;

pub use parser::{SpriteParser, SpriteTextureReference};
pub use processor::{DecodedSpriteTexture, SpriteProcessor};
pub use types::{
    Sprite, SpriteBorder, SpriteOffset, SpritePivot, SpriteRect, SpriteRenderData, SpriteSettings,
};
