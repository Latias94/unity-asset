//! Unity Sprite processing module
//!
//! This module provides comprehensive Sprite processing capabilities,
//! organized following UnityPy and unity-rs best practices.
//!
//! # Architecture
//!
//! The module is organized into several sub-modules:
//! - `types` - Core data structures (Sprite, SpriteRenderData, etc.)
//! - `parser` - Sprite parsing from Unity objects
//! - `processor` - High-level sprite processing and image extraction
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset_binary::sprite::{SpriteProcessor, SpriteConfig};
//! use unity_asset_binary::unity_version::UnityVersion;
//!
//! // Create processor with custom configuration
//! let version = UnityVersion::parse_version("2020.3.12f1")?;
//! let config = SpriteConfig {
//!     extract_images: true,
//!     process_atlas: true,
//!     max_sprite_size: Some((2048, 2048)),
//!     apply_transformations: true,
//! };
//! let processor = SpriteProcessor::with_config(version, config);
//!
//! // Process sprite from Unity object
//! let result = processor.parse_sprite(&sprite_object)?;
//! println!("Sprite name: {}", result.sprite.name);
//! # Ok::<(), unity_asset_binary::error::BinaryError>(())
//! ```

pub mod types;
pub mod parser;
pub mod processor;

// Re-export main types for easy access
pub use types::{
    // Core sprite types
    Sprite, SpriteRenderData, SpriteSettings, SpriteRect, SpriteOffset,
    SpritePivot, SpriteBorder, SpriteInfo, SpriteAtlas,
    // Configuration and results
    SpriteConfig, SpriteResult,
};
pub use parser::SpriteParser;
pub use processor::{SpriteProcessor, SpriteStats};

/// Main sprite processing facade
/// 
/// This struct provides a high-level interface for sprite processing,
/// combining parsing and processing functionality.
pub struct SpriteManager {
    processor: SpriteProcessor,
}

impl SpriteManager {
    /// Create a new sprite manager
    pub fn new(version: crate::unity_version::UnityVersion) -> Self {
        Self {
            processor: SpriteProcessor::new(version),
        }
    }

    /// Create a sprite manager with custom configuration
    pub fn with_config(version: crate::unity_version::UnityVersion, config: SpriteConfig) -> Self {
        Self {
            processor: SpriteProcessor::with_config(version, config),
        }
    }

    /// Process sprite from Unity object
    pub fn process_sprite(&self, object: &crate::object::UnityObject) -> crate::error::Result<SpriteResult> {
        self.processor.parse_sprite(object)
    }

    /// Process sprite with texture for image extraction
    pub fn process_sprite_with_texture(
        &self,
        sprite_object: &crate::object::UnityObject,
        texture: &crate::texture::Texture2D,
    ) -> crate::error::Result<SpriteResult> {
        self.processor.process_sprite_with_texture(sprite_object, texture)
    }

    /// Process multiple sprites as an atlas
    pub fn process_sprite_atlas(&self, sprites: &[&crate::object::UnityObject]) -> crate::error::Result<SpriteAtlas> {
        self.processor.process_sprite_atlas(sprites)
    }

    /// Get sprite statistics
    pub fn get_statistics(&self, sprites: &[&Sprite]) -> SpriteStats {
        self.processor.get_sprite_stats(sprites)
    }

    /// Validate sprite data
    pub fn validate_sprite(&self, sprite: &Sprite) -> crate::error::Result<()> {
        self.processor.validate_sprite(sprite)
    }

    /// Get supported features
    pub fn get_supported_features(&self) -> Vec<&'static str> {
        self.processor.get_supported_features()
    }

    /// Check if a feature is supported
    pub fn is_feature_supported(&self, feature: &str) -> bool {
        self.processor.is_feature_supported(feature)
    }

    /// Get the current configuration
    pub fn config(&self) -> &SpriteConfig {
        self.processor.config()
    }

    /// Set the configuration
    pub fn set_config(&mut self, config: SpriteConfig) {
        self.processor.set_config(config);
    }

    /// Get the Unity version
    pub fn version(&self) -> &crate::unity_version::UnityVersion {
        self.processor.version()
    }

    /// Set the Unity version
    pub fn set_version(&mut self, version: crate::unity_version::UnityVersion) {
        self.processor.set_version(version);
    }
}

impl Default for SpriteManager {
    fn default() -> Self {
        Self::new(crate::unity_version::UnityVersion::default())
    }
}

/// Convenience functions for common operations

/// Create a sprite manager with default settings
pub fn create_manager(version: crate::unity_version::UnityVersion) -> SpriteManager {
    SpriteManager::new(version)
}

/// Create a sprite manager optimized for performance
pub fn create_performance_manager(version: crate::unity_version::UnityVersion) -> SpriteManager {
    let config = SpriteConfig {
        extract_images: false,
        process_atlas: false,
        max_sprite_size: Some((1024, 1024)),
        apply_transformations: false,
    };
    SpriteManager::with_config(version, config)
}

/// Create a sprite manager with full features
pub fn create_full_manager(version: crate::unity_version::UnityVersion) -> SpriteManager {
    let config = SpriteConfig {
        extract_images: true,
        process_atlas: true,
        max_sprite_size: None,
        apply_transformations: true,
    };
    SpriteManager::with_config(version, config)
}

/// Parse sprite from Unity object (convenience function)
pub fn parse_sprite(
    object: &crate::object::UnityObject,
    version: &crate::unity_version::UnityVersion,
) -> crate::error::Result<Sprite> {
    let parser = SpriteParser::new(version.clone());
    let result = parser.parse_from_unity_object(object)?;
    Ok(result.sprite)
}

/// Extract sprite image from texture (convenience function)
pub fn extract_sprite_image(
    sprite: &Sprite,
    texture: &crate::texture::Texture2D,
    version: &crate::unity_version::UnityVersion,
) -> crate::error::Result<Vec<u8>> {
    let processor = SpriteProcessor::new(version.clone());
    processor.extract_sprite_image(sprite, texture)
}

/// Validate sprite data (convenience function)
pub fn validate_sprite(sprite: &Sprite) -> crate::error::Result<()> {
    let processor = SpriteProcessor::default();
    processor.validate_sprite(sprite)
}

/// Get sprite area in pixels
pub fn get_sprite_area(sprite: &Sprite) -> f32 {
    sprite.get_area()
}

/// Check if sprite is 9-slice
pub fn is_nine_slice_sprite(sprite: &Sprite) -> bool {
    sprite.has_border()
}

/// Check if sprite is from atlas
pub fn is_atlas_sprite(sprite: &Sprite) -> bool {
    sprite.is_atlas_sprite()
}

/// Get sprite aspect ratio
pub fn get_sprite_aspect_ratio(sprite: &Sprite) -> f32 {
    sprite.get_aspect_ratio()
}

/// Check if Unity version supports sprite feature
pub fn is_sprite_feature_supported(version: &crate::unity_version::UnityVersion, feature: &str) -> bool {
    match feature {
        "basic_sprite" | "rect" | "pivot" => true,
        "border" | "pixels_to_units" => version.major >= 5,
        "polygon_sprites" | "sprite_atlas" => version.major >= 2017,
        "sprite_mesh" | "sprite_physics" => version.major >= 2018,
        _ => false,
    }
}

/// Get recommended sprite configuration for Unity version
pub fn get_recommended_config(version: &crate::unity_version::UnityVersion) -> SpriteConfig {
    if version.major >= 2018 {
        // Modern Unity - full features
        SpriteConfig {
            extract_images: true,
            process_atlas: true,
            max_sprite_size: None,
            apply_transformations: true,
        }
    } else if version.major >= 2017 {
        // Unity 2017 - atlas support
        SpriteConfig {
            extract_images: true,
            process_atlas: true,
            max_sprite_size: Some((2048, 2048)),
            apply_transformations: true,
        }
    } else if version.major >= 5 {
        // Unity 5+ - basic features
        SpriteConfig {
            extract_images: true,
            process_atlas: false,
            max_sprite_size: Some((1024, 1024)),
            apply_transformations: false,
        }
    } else {
        // Legacy Unity - minimal features
        SpriteConfig {
            extract_images: false,
            process_atlas: false,
            max_sprite_size: Some((512, 512)),
            apply_transformations: false,
        }
    }
}

/// Sprite processing options
#[derive(Debug, Clone)]
pub struct ProcessingOptions {
    pub parallel_processing: bool,
    pub cache_results: bool,
    pub validate_sprites: bool,
    pub generate_thumbnails: bool,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            parallel_processing: false,
            cache_results: true,
            validate_sprites: true,
            generate_thumbnails: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_creation() {
        let version = crate::unity_version::UnityVersion::default();
        let manager = create_manager(version);
        assert!(manager.get_supported_features().contains(&"basic_sprite"));
    }

    #[test]
    fn test_performance_manager() {
        let version = crate::unity_version::UnityVersion::default();
        let manager = create_performance_manager(version);
        assert!(!manager.config().extract_images);
        assert!(!manager.config().process_atlas);
    }

    #[test]
    fn test_full_manager() {
        let version = crate::unity_version::UnityVersion::default();
        let manager = create_full_manager(version);
        assert!(manager.config().extract_images);
        assert!(manager.config().process_atlas);
    }

    #[test]
    fn test_feature_support() {
        let version_2020 = crate::unity_version::UnityVersion::parse_version("2020.3.12f1").unwrap();
        assert!(is_sprite_feature_supported(&version_2020, "basic_sprite"));
        assert!(is_sprite_feature_supported(&version_2020, "polygon_sprites"));
        assert!(is_sprite_feature_supported(&version_2020, "sprite_mesh"));

        let version_2017 = crate::unity_version::UnityVersion::parse_version("2017.4.40f1").unwrap();
        assert!(is_sprite_feature_supported(&version_2017, "sprite_atlas"));
        assert!(!is_sprite_feature_supported(&version_2017, "sprite_mesh"));
    }

    #[test]
    fn test_recommended_config() {
        let version_2020 = crate::unity_version::UnityVersion::parse_version("2020.3.12f1").unwrap();
        let config = get_recommended_config(&version_2020);
        assert!(config.extract_images);
        assert!(config.process_atlas);
        assert!(config.apply_transformations);

        let version_5 = crate::unity_version::UnityVersion::parse_version("5.6.7f1").unwrap();
        let config = get_recommended_config(&version_5);
        assert!(config.extract_images);
        assert!(!config.process_atlas);
    }
}
