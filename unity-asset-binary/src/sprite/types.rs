//! Sprite type definitions
//!
//! This module defines all the data structures used for Unity Sprite processing.

use serde::{Deserialize, Serialize};

/// Sprite render data
///
/// Contains information about how a sprite is rendered, including texture coordinates
/// and atlas information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpriteRenderData {
    pub texture_path_id: i64,
    pub texture_rect_x: f32,
    pub texture_rect_y: f32,
    pub texture_rect_width: f32,
    pub texture_rect_height: f32,
    pub texture_rect_offset_x: f32,
    pub texture_rect_offset_y: f32,
    pub atlas_rect_offset_x: f32,
    pub atlas_rect_offset_y: f32,
    pub downscale_multiplier: f32,
}

/// Sprite settings
///
/// Contains packing and mesh generation settings for sprites.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpriteSettings {
    pub packed: bool,
    pub packing_mode: i32,
    pub packing_rotation: i32,
    pub mesh_type: i32,
}

/// Sprite rectangle information
///
/// Defines the rectangular area of a sprite within its texture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Default for SpriteRect {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        }
    }
}

/// Sprite offset information
///
/// Defines the offset of a sprite from its original position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteOffset {
    pub x: f32,
    pub y: f32,
}

impl Default for SpriteOffset {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0 }
    }
}

/// Sprite pivot information
///
/// Defines the pivot point of a sprite (0,0 to 1,1 normalized coordinates).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpritePivot {
    pub x: f32,
    pub y: f32,
}

impl Default for SpritePivot {
    fn default() -> Self {
        Self { x: 0.5, y: 0.5 } // Center pivot by default
    }
}

/// Sprite border information (for 9-slice sprites)
///
/// Defines the border sizes for 9-slice sprite rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteBorder {
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

impl Default for SpriteBorder {
    fn default() -> Self {
        Self {
            left: 0.0,
            bottom: 0.0,
            right: 0.0,
            top: 0.0,
        }
    }
}

/// Comprehensive sprite information
///
/// Contains all the information needed to fully describe a sprite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteInfo {
    pub name: String,
    pub rect: SpriteRect,
    pub offset: SpriteOffset,
    pub pivot: SpritePivot,
    pub border: SpriteBorder,
    pub pixels_to_units: f32,
    pub is_polygon: bool,
    pub texture_path_id: i64,
    pub is_atlas_sprite: bool,
}

impl Default for SpriteInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            rect: SpriteRect::default(),
            offset: SpriteOffset::default(),
            pivot: SpritePivot::default(),
            border: SpriteBorder::default(),
            pixels_to_units: 100.0,
            is_polygon: false,
            texture_path_id: 0,
            is_atlas_sprite: false,
        }
    }
}

/// Sprite object representation
///
/// Main sprite structure containing all sprite data and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sprite {
    pub name: String,
    pub rect_x: f32,
    pub rect_y: f32,
    pub rect_width: f32,
    pub rect_height: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub border_x: f32,
    pub border_y: f32,
    pub border_z: f32,
    pub border_w: f32,
    pub pixels_to_units: f32,
    pub pivot_x: f32,
    pub pivot_y: f32,
    pub extrude: u8,
    pub is_polygon: bool,
    pub render_data: SpriteRenderData,
    pub settings: SpriteSettings,

    // Atlas reference
    pub atlas_tags: Vec<String>,
    pub sprite_atlas_path_id: Option<i64>,
}

impl Default for Sprite {
    fn default() -> Self {
        Self {
            name: String::new(),
            rect_x: 0.0,
            rect_y: 0.0,
            rect_width: 0.0,
            rect_height: 0.0,
            offset_x: 0.0,
            offset_y: 0.0,
            border_x: 0.0,
            border_y: 0.0,
            border_z: 0.0,
            border_w: 0.0,
            pixels_to_units: 100.0,
            pivot_x: 0.5,
            pivot_y: 0.5,
            extrude: 1,
            is_polygon: false,
            render_data: SpriteRenderData::default(),
            settings: SpriteSettings::default(),
            atlas_tags: Vec::new(),
            sprite_atlas_path_id: None,
        }
    }
}

/// Sprite processing configuration
#[derive(Debug, Clone)]
pub struct SpriteConfig {
    /// Whether to extract sprite images
    pub extract_images: bool,
    /// Whether to process atlas sprites
    pub process_atlas: bool,
    /// Maximum sprite size to process
    pub max_sprite_size: Option<(u32, u32)>,
    /// Whether to apply sprite transformations
    pub apply_transformations: bool,
}

impl Default for SpriteConfig {
    fn default() -> Self {
        Self {
            extract_images: true,
            process_atlas: true,
            max_sprite_size: None,
            apply_transformations: true,
        }
    }
}

/// Sprite processing result
#[derive(Debug, Clone)]
pub struct SpriteResult {
    pub sprite: Sprite,
    pub image_data: Option<Vec<u8>>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl SpriteResult {
    pub fn new(sprite: Sprite) -> Self {
        Self {
            sprite,
            image_data: None,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn with_image(mut self, image_data: Vec<u8>) -> Self {
        self.image_data = Some(image_data);
        self
    }

    pub fn add_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    pub fn add_error(&mut self, error: String) {
        self.errors.push(error);
    }

    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn has_image(&self) -> bool {
        self.image_data.is_some()
    }
}

/// Sprite atlas information
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct SpriteAtlas {
    pub name: String,
    pub texture_path_id: i64,
    pub sprites: Vec<SpriteInfo>,
    pub packed_sprites: Vec<String>,
}


/// Helper functions for sprite types
impl Sprite {
    /// Get sprite rectangle as SpriteRect
    pub fn get_rect(&self) -> SpriteRect {
        SpriteRect {
            x: self.rect_x,
            y: self.rect_y,
            width: self.rect_width,
            height: self.rect_height,
        }
    }

    /// Get sprite offset as SpriteOffset
    pub fn get_offset(&self) -> SpriteOffset {
        SpriteOffset {
            x: self.offset_x,
            y: self.offset_y,
        }
    }

    /// Get sprite pivot as SpritePivot
    pub fn get_pivot(&self) -> SpritePivot {
        SpritePivot {
            x: self.pivot_x,
            y: self.pivot_y,
        }
    }

    /// Get sprite border as SpriteBorder
    pub fn get_border(&self) -> SpriteBorder {
        SpriteBorder {
            left: self.border_x,
            bottom: self.border_y,
            right: self.border_z,
            top: self.border_w,
        }
    }

    /// Check if sprite has border (is 9-slice)
    pub fn has_border(&self) -> bool {
        self.border_x > 0.0 || self.border_y > 0.0 || self.border_z > 0.0 || self.border_w > 0.0
    }

    /// Check if sprite is from an atlas
    pub fn is_atlas_sprite(&self) -> bool {
        self.sprite_atlas_path_id.is_some()
    }

    /// Get sprite area in pixels
    pub fn get_area(&self) -> f32 {
        self.rect_width * self.rect_height
    }

    /// Get sprite aspect ratio
    pub fn get_aspect_ratio(&self) -> f32 {
        if self.rect_height > 0.0 {
            self.rect_width / self.rect_height
        } else {
            1.0
        }
    }
}

impl SpriteRect {
    /// Check if rectangle contains a point
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }

    /// Get rectangle area
    pub fn area(&self) -> f32 {
        self.width * self.height
    }

    /// Get rectangle center point
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}
