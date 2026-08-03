//! Strict Unity Sprite inspection, rendering, and caller-owned encoding.

mod inspection;
mod prepared;
pub mod processor;
pub mod types;

pub use inspection::{SpriteLayout, SpritePixelRect, SpriteTextureReference};
pub use prepared::{PreparedSpritePng, SpritePreparationError};
pub use processor::{DecodedSpriteTexture, SpriteProcessor};
pub use types::{
    Sprite, SpriteBorder, SpriteOffset, SpritePivot, SpriteRect, SpriteRenderData, SpriteSettings,
};
