//! Sprite processing implementation
//!
//! This module provides high-level sprite parsing, rendering, and caller-owned
//! PNG encoding.

use super::parser::SpriteParser;
use super::types::*;
use crate::error::{BinaryError, Result};
use crate::object::UnityObject;
use crate::texture::Texture2D;
use crate::texture::helpers::export::write_rgba_png;
use image::{RgbaImage, imageops};
use std::io::Write;

/// Sprite processor
///
/// This struct provides high-level methods for processing Unity Sprite objects,
/// including parsing and caller-owned image encoding.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpriteProcessor;

/// Opaque decoded texture shared while rendering sprites from one atlas.
#[derive(Debug)]
pub struct DecodedSpriteTexture {
    image: RgbaImage,
}

impl SpriteProcessor {
    /// Create a new Sprite processor
    pub const fn new() -> Self {
        Self
    }

    /// Parse Sprite from Unity object
    pub fn parse_sprite(&self, object: &UnityObject) -> Result<Sprite> {
        SpriteParser::new().parse_from_unity_object(object)
    }

    /// Extract and encode a sprite as PNG into a caller-owned sink.
    ///
    /// The sink is not flushed. Callers that buffer output retain control over
    /// the flush and durability policy.
    pub fn write_sprite_png<W: Write + ?Sized>(
        &self,
        sprite: &Sprite,
        texture: &Texture2D,
        writer: &mut W,
    ) -> Result<()> {
        let image = self.render_sprite(sprite, texture)?;
        write_rgba_png(&image, writer, "Failed to encode PNG")
    }

    /// Render a sprite into a standalone RGBA image.
    ///
    /// Keeping rendering separate from PNG publication lets callers preserve
    /// the distinction between malformed Unity data and a failing output sink.
    pub fn render_sprite(&self, sprite: &Sprite, texture: &Texture2D) -> Result<RgbaImage> {
        let texture = self.decode_sprite_texture(texture)?;
        self.render_sprite_from_texture(sprite, &texture)
    }

    /// Decode one Texture2D for reuse by every Sprite that references it.
    pub fn decode_sprite_texture(&self, texture: &Texture2D) -> Result<DecodedSpriteTexture> {
        let converter = crate::texture::Texture2DConverter::new();
        converter
            .decode_to_image(texture)
            .map(|image| DecodedSpriteTexture { image })
    }

    /// Render a Sprite from an already decoded atlas texture.
    pub fn render_sprite_from_texture(
        &self,
        sprite: &Sprite,
        texture: &DecodedSpriteTexture,
    ) -> Result<RgbaImage> {
        let texture_image = &texture.image;

        // Calculate sprite bounds
        let sprite_rect = sprite.get_rect();
        let texture_width = texture_image.width();
        let texture_height = texture_image.height();

        // Validate sprite bounds
        if sprite_rect.x < 0.0
            || sprite_rect.y < 0.0
            || sprite_rect.x + sprite_rect.width > texture_width as f32
            || sprite_rect.y + sprite_rect.height > texture_height as f32
        {
            return Err(BinaryError::invalid_data(
                "Sprite rect is outside texture bounds",
            ));
        }

        // Extract sprite region
        let x = sprite_rect.x as u32;
        let y = sprite_rect.y as u32;
        let width = sprite_rect.width as u32;
        let height = sprite_rect.height as u32;

        // Unity uses bottom-left origin, but image crate uses top-left
        // So we need to flip the Y coordinate
        let flipped_y = texture_height - y - height;

        Ok(imageops::crop_imm(texture_image, x, flipped_y, width, height).to_image())
    }
}
