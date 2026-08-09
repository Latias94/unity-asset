//! Strict Unity Sprite inspection, rendering, and caller-owned encoding.

mod inspection;
mod prepared;

pub use inspection::{SpriteLayout, SpritePixelRect, SpriteTextureReference};
pub use prepared::{PreparedSpritePng, SpritePreparationError};
