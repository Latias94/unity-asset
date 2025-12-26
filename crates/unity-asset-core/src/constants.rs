//! Constants and type definitions for Unity YAML parsing
//!
//! This module contains Unity-specific constants, tags, and type definitions
//! that are used throughout the parsing process.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Unity YAML tag URI
pub const UNITY_TAG_URI: &str = "tag:unity3d.com,2011:";

/// Unity YAML version
pub const UNITY_YAML_VERSION: (u32, u32) = (1, 1);

/// Line ending types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Unix,    // \n
    Windows, // \r\n
    Mac,     // \r
}

impl Default for LineEnding {
    fn default() -> Self {
        #[cfg(windows)]
        return LineEnding::Windows;
        #[cfg(not(windows))]
        return LineEnding::Unix;
    }
}

impl LineEnding {
    pub fn as_str(&self) -> &'static str {
        match self {
            LineEnding::Unix => "\n",
            LineEnding::Windows => "\r\n",
            LineEnding::Mac => "\r",
        }
    }

    /// Create LineEnding from string representation
    pub fn from_string(s: &str) -> Self {
        match s {
            "\n" => LineEnding::Unix,
            "\r\n" => LineEnding::Windows,
            "\r" => LineEnding::Mac,
            _ => LineEnding::default(),
        }
    }
}

/// Unity class ID to name mapping
/// This is a global registry that maps class IDs to class names
pub struct UnityClassIdMap {
    map: Arc<RwLock<HashMap<String, String>>>,
}

impl UnityClassIdMap {
    pub fn new() -> Self {
        Self {
            map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a class mapping
    pub fn get_or_create(&self, class_id: &str, class_name: &str) -> String {
        let key = format!("{}-{}", class_id, class_name);

        // Try to read first
        if let Ok(map) = self.map.read()
            && let Some(existing) = map.get(&key)
        {
            return existing.clone();
        }

        // Need to write
        if let Ok(mut map) = self.map.write() {
            map.insert(key.clone(), class_name.to_string());
        }

        class_name.to_string()
    }

    /// Clear all mappings
    pub fn clear(&self) {
        if let Ok(mut map) = self.map.write() {
            map.clear();
        }
    }

    /// Get class name from class ID.
    pub fn get_class_name(&self, class_id: i32) -> Option<String> {
        self.get_class_name_str(class_id).map(str::to_string)
    }

    /// Get class name from class ID without allocating.
    pub fn get_class_name_str(&self, class_id: i32) -> Option<&'static str> {
        class_id_name(class_id)
    }
}

fn class_id_name(class_id: i32) -> Option<&'static str> {
    match class_id {
        // Core Unity objects
        0 => Some("Object"),
        1 => Some("GameObject"),
        2 => Some("Component"),
        4 => Some("Transform"),
        8 => Some("Behaviour"),

        // Managers
        3 => Some("LevelGameManager"),
        5 => Some("TimeManager"),
        6 => Some("GlobalGameManager"),
        9 => Some("GameManager"),
        11 => Some("AudioManager"),
        13 => Some("InputManager"),

        // Rendering
        20 => Some("Camera"),
        21 => Some("Material"),
        23 => Some("MeshRenderer"),
        25 => Some("Renderer"),
        27 => Some("Texture"),
        28 => Some("Texture2D"),
        33 => Some("MeshFilter"),
        43 => Some("Mesh"),
        48 => Some("Shader"),

        // Text and Assets
        49 => Some("TextAsset"),
        74 => Some("AnimationClip"),
        83 => Some("AudioClip"),
        89 => Some("CubemapArray"),
        90 => Some("Avatar"),
        91 => Some("AnimatorController"),
        95 => Some("Animator"),
        108 => Some("Light"),
        114 => Some("MonoBehaviour"),
        115 => Some("MonoScript"),
        128 => Some("Font"),
        142 => Some("AssetBundle"),
        152 => Some("MovieTexture"),
        184 => Some("RenderTexture"),
        212 => Some("SpriteRenderer"),
        213 => Some("Sprite"),
        1001 => Some("PrefabInstance"),

        // Physics
        50 => Some("Rigidbody2D"),
        54 => Some("Rigidbody"),
        56 => Some("Collider"),

        // Editor / additional types
        687078895 => Some("SpriteAtlas"),

        _ => None,
    }
}

impl Default for UnityClassIdMap {
    fn default() -> Self {
        Self::new()
    }
}

lazy_static::lazy_static! {
    /// Global class ID map instance
    pub static ref GLOBAL_CLASS_ID_MAP: UnityClassIdMap = UnityClassIdMap::new();
}

/// Common Unity class IDs
pub mod class_ids {
    pub const OBJECT: i32 = 0;
    pub const GAME_OBJECT: i32 = 1;
    pub const COMPONENT: i32 = 2;
    pub const BEHAVIOUR: i32 = 8;
    pub const TRANSFORM: i32 = 4;
    pub const CAMERA: i32 = 20;
    pub const MATERIAL: i32 = 21;
    pub const MESH_RENDERER: i32 = 23;
    pub const TEXTURE_2D: i32 = 28;
    pub const MESH: i32 = 43;
    pub const SHADER: i32 = 48;
    pub const TEXTURE: i32 = 27;
    pub const TEXT_ASSET: i32 = 49;
    pub const ANIMATION_CLIP: i32 = 74;
    pub const AUDIO_CLIP: i32 = 83;
    pub const ANIMATOR_CONTROLLER: i32 = 91;
    pub const MONO_BEHAVIOUR: i32 = 114;
    pub const MONO_SCRIPT: i32 = 115;
    pub const ASSET_BUNDLE: i32 = 142;
    pub const SPRITE_RENDERER: i32 = 212;
    pub const SPRITE: i32 = 213;
    pub const PREFAB_INSTANCE: i32 = 1001;
    pub const SPRITE_ATLAS: i32 = 687078895;
}

/// Common Unity class names
pub mod class_names {
    pub const OBJECT: &str = "Object";
    pub const GAME_OBJECT: &str = "GameObject";
    pub const COMPONENT: &str = "Component";
    pub const TRANSFORM: &str = "Transform";
    pub const CAMERA: &str = "Camera";
    pub const MATERIAL: &str = "Material";
    pub const MESH_RENDERER: &str = "MeshRenderer";
    pub const TEXTURE_2D: &str = "Texture2D";
    pub const MESH: &str = "Mesh";
    pub const SHADER: &str = "Shader";
    pub const TEXTURE: &str = "Texture";
    pub const SPRITE: &str = "Sprite";
    pub const MONO_BEHAVIOUR: &str = "MonoBehaviour";
    pub const MONO_SCRIPT: &str = "MonoScript";
    pub const PREFAB_INSTANCE: &str = "PrefabInstance";
    pub const SPRITE_ATLAS: &str = "SpriteAtlas";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_ending() {
        assert_eq!(LineEnding::Unix.as_str(), "\n");
        assert_eq!(LineEnding::Windows.as_str(), "\r\n");
        assert_eq!(LineEnding::Mac.as_str(), "\r");
    }

    #[test]
    fn test_common_class_ids() {
        assert_eq!(class_ids::OBJECT, 0);
        assert_eq!(class_ids::GAME_OBJECT, 1);
        assert_eq!(class_ids::COMPONENT, 2);
        assert_eq!(class_ids::TRANSFORM, 4);
        assert_eq!(class_ids::BEHAVIOUR, 8);
        assert_eq!(class_ids::SPRITE_RENDERER, 212);
        assert_eq!(class_ids::SPRITE, 213);

        assert_eq!(
            GLOBAL_CLASS_ID_MAP.get_class_name(class_ids::SPRITE),
            Some("Sprite".to_string())
        );
        assert_eq!(
            GLOBAL_CLASS_ID_MAP.get_class_name(class_ids::SPRITE_RENDERER),
            Some("SpriteRenderer".to_string())
        );

        // Defensive: avoid "guess" mappings for unknown IDs.
        assert_eq!(GLOBAL_CLASS_ID_MAP.get_class_name(256), None);
        assert_eq!(GLOBAL_CLASS_ID_MAP.get_class_name(512), None);
        assert_eq!(GLOBAL_CLASS_ID_MAP.get_class_name(768), None);
    }

    #[test]
    fn test_class_id_map() {
        let map = UnityClassIdMap::new();
        let result = map.get_or_create("1", "GameObject");
        assert_eq!(result, "GameObject");

        // Should return the same result
        let result2 = map.get_or_create("1", "GameObject");
        assert_eq!(result2, "GameObject");
    }
}
