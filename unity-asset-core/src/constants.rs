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

    /// Get class name from class ID
    pub fn get_class_name(&self, class_id: i32) -> Option<String> {
        // Unity class ID to name mapping (based on UnityPy and unity-rs)
        match class_id {
            // Core Unity objects
            0 => Some("Object".to_string()),
            1 => Some("GameObject".to_string()),
            2 => Some("Component".to_string()),
            4 => Some("Transform".to_string()),
            8 => Some("Behaviour".to_string()),

            // Managers
            3 => Some("LevelGameManager".to_string()),
            5 => Some("TimeManager".to_string()),
            6 => Some("GlobalGameManager".to_string()),
            9 => Some("GameManager".to_string()),
            11 => Some("AudioManager".to_string()),
            13 => Some("InputManager".to_string()),

            // Rendering
            20 => Some("Camera".to_string()),
            21 => Some("Material".to_string()),
            23 => Some("MeshRenderer".to_string()),
            25 => Some("Renderer".to_string()),
            27 => Some("Texture".to_string()),
            28 => Some("Texture2D".to_string()),
            33 => Some("MeshFilter".to_string()),
            43 => Some("Mesh".to_string()),
            48 => Some("Shader".to_string()),

            // Text and Assets
            49 => Some("TextAsset".to_string()),
            74 => Some("AnimationClip".to_string()),
            83 => Some("AudioClip".to_string()),
            89 => Some("CubemapArray".to_string()),
            90 => Some("Avatar".to_string()),
            91 => Some("AnimatorController".to_string()),
            95 => Some("Animator".to_string()),
            108 => Some("Light".to_string()),
            114 => Some("MonoBehaviour".to_string()),
            115 => Some("MonoScript".to_string()),
            128 => Some("Font".to_string()),
            142 => Some("AssetBundle".to_string()),
            152 => Some("MovieTexture".to_string()),
            184 => Some("RenderTexture".to_string()),
            212 => Some("SpriteRenderer".to_string()),
            213 => Some("Sprite".to_string()),
            1001 => Some("PrefabInstance".to_string()),

            // Physics
            50 => Some("Rigidbody2D".to_string()),
            54 => Some("Rigidbody".to_string()),
            56 => Some("Collider".to_string()),

            // Special/Unknown types that we've encountered
            687078895 => Some("SpriteAtlas".to_string()),

            _ => None,
        }
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
