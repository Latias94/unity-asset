//! Unity YAML loader based on serde_yaml
//!
//! This module provides a more robust Unity YAML loader that uses the mature
//! serde_yaml library as its foundation and adds Unity-specific extensions.

use crate::Result;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_yaml::Value;
use std::io::Read;
use unity_asset_core::{UnityAssetError, UnityClass, UnityValue};

#[cfg(feature = "async")]
use tokio::io::{AsyncRead, AsyncReadExt};

/// Unity YAML loader based on serde_yaml
pub struct SerdeUnityLoader;

impl SerdeUnityLoader {
    /// Create a new serde-based Unity loader
    pub fn new() -> Self {
        Self
    }

    /// Load Unity YAML from a reader
    pub fn load_from_reader<R: Read>(&self, mut reader: R) -> Result<Vec<UnityClass>> {
        // Read the entire content first
        let mut content = String::new();
        reader
            .read_to_string(&mut content)
            .map_err(|e| UnityAssetError::parse(format!("Failed to read input: {}", e)))?;

        // Preprocess Unity YAML to handle Unity-specific features
        let processed_content = self.preprocess_unity_yaml(&content)?;

        // Parse YAML using serde_yaml
        let documents: Vec<Value> = serde_yaml::Deserializer::from_str(&processed_content)
            .map(Value::deserialize)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| UnityAssetError::parse(format!("YAML parsing error: {}", e)))?;

        // Convert each document to UnityClass
        let mut unity_classes = Vec::new();
        for (doc_index, document) in documents.iter().enumerate() {
            match self.convert_document_to_unity_class(document, doc_index) {
                Ok(unity_class) => unity_classes.push(unity_class),
                Err(e) => {
                    // Log error but continue processing other documents
                    eprintln!("Warning: Failed to convert document {}: {}", doc_index, e);
                }
            }
        }

        Ok(unity_classes)
    }

    /// Load Unity YAML from a string
    pub fn load_from_str(&self, yaml_str: &str) -> Result<Vec<UnityClass>> {
        use std::io::Cursor;
        let cursor = Cursor::new(yaml_str.as_bytes());
        self.load_from_reader(cursor)
    }

    /// Load Unity YAML from an async reader
    #[cfg(feature = "async")]
    pub async fn load_from_async_reader<R: AsyncRead + Unpin>(
        &self,
        mut reader: R,
    ) -> Result<Vec<UnityClass>> {
        // Read the entire content first
        let mut content = String::new();
        reader
            .read_to_string(&mut content)
            .await
            .map_err(|e| UnityAssetError::parse(format!("Failed to read input: {}", e)))?;

        // Use the existing string processing logic
        self.load_from_str(&content)
    }

    /// Preprocess Unity YAML to handle Unity-specific features
    fn preprocess_unity_yaml(&self, content: &str) -> Result<String> {
        let mut processed = String::new();
        let mut in_document = false;
        let mut current_class_info: Option<(i32, String)> = None;

        for line in content.lines() {
            let trimmed = line.trim();

            // Handle YAML directives
            if trimmed.starts_with('%') {
                processed.push_str(line);
                processed.push('\n');
                continue;
            }

            // Handle document separators
            if trimmed.starts_with("---") {
                in_document = true;

                // Parse Unity document header: --- !u!129 &1
                if let Some(unity_info) = self.parse_unity_document_header(trimmed) {
                    current_class_info = Some(unity_info);
                    // Convert to standard YAML document separator
                    processed.push_str("---\n");
                } else {
                    processed.push_str(line);
                    processed.push('\n');
                }
                continue;
            }

            // Handle the first line after document separator (class name)
            if in_document
                && !trimmed.is_empty()
                && !trimmed.starts_with(' ')
                && trimmed.ends_with(':')
            {
                if let Some((class_id, anchor)) = &current_class_info {
                    // Add Unity metadata as special properties
                    let class_name = trimmed.trim_end_matches(':');
                    processed.push_str(&format!("{}:\n", class_name));
                    processed.push_str(&format!("  __unity_class_id__: {}\n", class_id));
                    processed.push_str(&format!("  __unity_anchor__: \"{}\"\n", anchor));
                    current_class_info = None;
                } else {
                    processed.push_str(line);
                    processed.push('\n');
                }
                continue;
            }

            // Regular line
            processed.push_str(line);
            processed.push('\n');
        }

        Ok(processed)
    }

    /// Parse Unity document header like "--- !u!129 &1"
    fn parse_unity_document_header(&self, line: &str) -> Option<(i32, String)> {
        let parts: Vec<&str> = line.split_whitespace().collect();

        let mut class_id = 0;
        let mut anchor = "0".to_string();

        for part in parts {
            if let Some(stripped) = part.strip_prefix("!u!") {
                if let Ok(id) = stripped.parse::<i32>() {
                    class_id = id;
                }
            } else if let Some(stripped) = part.strip_prefix('&') {
                anchor = stripped.to_string();
            }
        }

        if class_id > 0 {
            Some((class_id, anchor))
        } else {
            None
        }
    }

    /// Convert a YAML document to UnityClass
    fn convert_document_to_unity_class(
        &self,
        document: &Value,
        doc_index: usize,
    ) -> Result<UnityClass> {
        match document {
            Value::Mapping(mapping) => {
                // Look for Unity class structure
                if let Some((class_key, class_value)) = mapping.iter().next() {
                    let class_name = match class_key {
                        Value::String(s) => s.clone(),
                        _ => format!("Unknown_{}", doc_index),
                    };

                    // Extract Unity metadata from the class properties
                    let (class_id, anchor, properties) =
                        if let Value::Mapping(class_props) = class_value {
                            let mut class_id = 0;
                            let mut anchor = format!("doc_{}", doc_index);
                            let mut filtered_props = IndexMap::new();

                            for (key, value) in class_props {
                                if let Value::String(key_str) = key {
                                    match key_str.as_str() {
                                        "__unity_class_id__" => {
                                            if let Value::Number(n) = value
                                                && let Some(id) = n.as_i64() {
                                                    class_id = id as i32;
                                                }
                                        }
                                        "__unity_anchor__" => {
                                            if let Value::String(a) = value {
                                                anchor = a.clone();
                                            }
                                        }
                                        _ => {
                                            // Regular property
                                            let unity_value =
                                                Self::convert_value_to_unity_value(value)?;
                                            filtered_props.insert(key_str.clone(), unity_value);
                                        }
                                    }
                                }
                            }

                            (class_id, anchor, UnityValue::Object(filtered_props))
                        } else {
                            let properties = Self::convert_value_to_unity_value(class_value)?;
                            (0, format!("doc_{}", doc_index), properties)
                        };

                    // Always use the actual class name from YAML - it's more reliable than ID mapping
                    // Unity class IDs can map to different names in different Unity versions
                    let final_class_name = class_name;

                    let mut unity_class = UnityClass::new(class_id, final_class_name, anchor);

                    // Add properties
                    if let UnityValue::Object(props) = properties {
                        for (key, value) in props {
                            unity_class.set(key, value);
                        }
                    }

                    Ok(unity_class)
                } else {
                    // Empty mapping, create a default UnityClass
                    Ok(UnityClass::new(
                        0,
                        "Unknown".to_string(),
                        format!("doc_{}", doc_index),
                    ))
                }
            }
            _ => {
                // Non-mapping document, treat as scalar
                let anchor = format!("doc_{}", doc_index);
                let mut unity_class = UnityClass::new(0, "Scalar".to_string(), anchor);
                let value = Self::convert_value_to_unity_value(document)?;
                unity_class.set("value".to_string(), value);
                Ok(unity_class)
            }
        }
    }

    /// Convert serde_yaml Value to UnityValue
    fn convert_value_to_unity_value(value: &Value) -> Result<UnityValue> {
        match value {
            Value::Null => Ok(UnityValue::Null),
            Value::Bool(b) => Ok(UnityValue::Bool(*b)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(UnityValue::Integer(i))
                } else if let Some(f) = n.as_f64() {
                    Ok(UnityValue::Float(f))
                } else {
                    Ok(UnityValue::String(n.to_string()))
                }
            }
            Value::String(s) => Ok(UnityValue::String(s.clone())),
            Value::Sequence(seq) => {
                let mut array = Vec::new();
                for item in seq {
                    array.push(Self::convert_value_to_unity_value(item)?);
                }
                Ok(UnityValue::Array(array))
            }
            Value::Mapping(mapping) => {
                let mut object = IndexMap::new();
                for (k, v) in mapping {
                    let key = match k {
                        Value::String(s) => s.clone(),
                        _ => format!("{:?}", k),
                    };
                    let value = Self::convert_value_to_unity_value(v)?;
                    object.insert(key, value);
                }
                Ok(UnityValue::Object(object))
            }
            Value::Tagged(tagged) => {
                // Handle tagged values
                Self::convert_value_to_unity_value(&tagged.value)
            }
        }
    }

    /// Get Unity class name from class ID
    /// Based on Unity's official ClassIDReference and reference libraries
    #[allow(dead_code)]
    fn get_class_name_from_id(&self, class_id: i32) -> String {
        match class_id {
            // Core runtime classes
            0 => "Object".to_string(),
            1 => "GameObject".to_string(),
            2 => "Component".to_string(),
            3 => "LevelGameManager".to_string(),
            4 => "Transform".to_string(),
            5 => "TimeManager".to_string(),
            6 => "GlobalGameManager".to_string(),
            8 => "Behaviour".to_string(),
            9 => "GameManager".to_string(),
            11 => "AudioManager".to_string(),
            12 => "ParticleAnimator".to_string(),
            13 => "InputManager".to_string(),
            15 => "EllipsoidParticleEmitter".to_string(),
            17 => "Pipeline".to_string(),
            18 => "EditorExtension".to_string(),
            19 => "Physics2DSettings".to_string(),
            20 => "Camera".to_string(),
            21 => "Material".to_string(),
            23 => "MeshRenderer".to_string(),
            25 => "Renderer".to_string(),
            26 => "ParticleRenderer".to_string(),
            27 => "Texture".to_string(),
            28 => "Texture2D".to_string(),
            29 => "OcclusionCullingSettings".to_string(),
            30 => "GraphicsSettings".to_string(),
            33 => "MeshFilter".to_string(),
            41 => "OcclusionPortal".to_string(),
            43 => "Mesh".to_string(),
            45 => "Skybox".to_string(),
            47 => "QualitySettings".to_string(),
            48 => "Shader".to_string(),
            49 => "TextAsset".to_string(),
            50 => "Rigidbody2D".to_string(),
            51 => "Physics2DManager".to_string(),
            53 => "Collider2D".to_string(),
            54 => "Rigidbody".to_string(),
            55 => "PhysicsManager".to_string(),
            56 => "Collider".to_string(),
            57 => "Joint".to_string(),
            58 => "CircleCollider2D".to_string(),
            59 => "HingeJoint".to_string(),
            60 => "PolygonCollider2D".to_string(),
            61 => "BoxCollider2D".to_string(),
            62 => "PhysicsMaterial2D".to_string(),
            64 => "MeshCollider".to_string(),
            65 => "BoxCollider".to_string(),
            68 => "EdgeCollider2D".to_string(),
            70 => "CapsuleCollider2D".to_string(),
            72 => "ComputeShader".to_string(),
            74 => "AnimationClip".to_string(),
            75 => "ConstantForce".to_string(),
            78 => "TagManager".to_string(),
            81 => "AudioListener".to_string(),
            82 => "AudioSource".to_string(),
            83 => "AudioClip".to_string(),
            84 => "RenderTexture".to_string(),
            86 => "CustomRenderTexture".to_string(),
            89 => "Cubemap".to_string(),
            90 => "Avatar".to_string(),
            91 => "AnimatorController".to_string(),
            92 => "GUILayer".to_string(),
            93 => "RuntimeAnimatorController".to_string(),
            94 => "ScriptMapper".to_string(),
            95 => "Animator".to_string(),
            96 => "TrailRenderer".to_string(),
            98 => "DelayedCallManager".to_string(),
            102 => "TextMesh".to_string(),
            104 => "RenderSettings".to_string(),
            108 => "Light".to_string(),
            109 => "CGProgram".to_string(),
            110 => "BaseAnimationTrack".to_string(),
            111 => "Animation".to_string(),
            114 => "MonoBehaviour".to_string(),
            115 => "MonoScript".to_string(),
            116 => "MonoManager".to_string(),
            117 => "Texture3D".to_string(),
            118 => "NewAnimationTrack".to_string(),
            119 => "Projector".to_string(),
            120 => "LineRenderer".to_string(),
            121 => "Flare".to_string(),
            122 => "Halo".to_string(),
            123 => "LensFlare".to_string(),
            124 => "FlareLayer".to_string(),
            125 => "HaloLayer".to_string(),
            126 => "NavMeshAreas".to_string(),
            127 => "HaloManager".to_string(),
            128 => "Font".to_string(),
            129 => "PlayerSettings".to_string(),
            130 => "NamedObject".to_string(),
            131 => "GUITexture".to_string(),
            132 => "GUIText".to_string(),
            133 => "GUIElement".to_string(),
            134 => "PhysicMaterial".to_string(),
            135 => "SphereCollider".to_string(),
            136 => "CapsuleCollider".to_string(),
            137 => "SkinnedMeshRenderer".to_string(),
            138 => "FixedJoint".to_string(),
            141 => "BuildSettings".to_string(),
            142 => "AssetBundle".to_string(),
            143 => "CharacterController".to_string(),
            144 => "CharacterJoint".to_string(),
            145 => "SpringJoint".to_string(),
            146 => "WheelCollider".to_string(),
            147 => "ResourceManager".to_string(),
            148 => "NetworkView".to_string(),
            149 => "NetworkManager".to_string(),
            150 => "EllipsoidParticleEmitter".to_string(),
            151 => "ParticleEmitter".to_string(),
            152 => "ParticleSystem".to_string(),
            153 => "ParticleSystemRenderer".to_string(),
            154 => "ShaderVariantCollection".to_string(),
            156 => "LODGroup".to_string(),
            157 => "BlendTree".to_string(),
            158 => "Motion".to_string(),
            159 => "NavMeshObstacle".to_string(),
            160 => "TerrainCollider".to_string(),
            161 => "TerrainData".to_string(),
            162 => "LightmapSettings".to_string(),
            163 => "WebCamTexture".to_string(),
            164 => "EditorSettings".to_string(),
            165 => "InteractiveCloth".to_string(),
            166 => "ClothRenderer".to_string(),
            167 => "EditorUserSettings".to_string(),
            168 => "SkinnedCloth".to_string(),
            180 => "AudioReverbFilter".to_string(),
            181 => "AudioHighPassFilter".to_string(),
            182 => "AudioChorusFilter".to_string(),
            183 => "AudioReverbZone".to_string(),
            184 => "AudioEchoFilter".to_string(),
            185 => "AudioLowPassFilter".to_string(),
            186 => "AudioDistortionFilter".to_string(),
            187 => "SparseTexture".to_string(),
            188 => "AudioBehaviour".to_string(),
            189 => "AudioFilter".to_string(),
            191 => "WindZone".to_string(),
            192 => "Cloth".to_string(),
            193 => "SubstanceArchive".to_string(),
            194 => "ProceduralMaterial".to_string(),
            195 => "ProceduralTexture".to_string(),
            196 => "Texture2DArray".to_string(),
            197 => "CubemapArray".to_string(),
            198 => "OffMeshLink".to_string(),
            199 => "OcclusionArea".to_string(),
            200 => "Tree".to_string(),
            201 => "NavMeshAgent".to_string(),
            202 => "NavMeshSettings".to_string(),
            203 => "LightProbesLegacy".to_string(),
            204 => "ParticleSystemForceField".to_string(),
            205 => "OcclusionCullingData".to_string(),
            206 => "NavMeshData".to_string(),
            207 => "AudioMixer".to_string(),
            208 => "AudioMixerController".to_string(),
            210 => "AudioMixerGroupController".to_string(),
            211 => "AudioMixerEffectController".to_string(),
            212 => "AudioMixerSnapshotController".to_string(),
            213 => "PhysicsUpdateBehaviour2D".to_string(),
            214 => "ConstantForce2D".to_string(),
            215 => "Effector2D".to_string(),
            216 => "AreaEffector2D".to_string(),
            217 => "PointEffector2D".to_string(),
            218 => "PlatformEffector2D".to_string(),
            219 => "SurfaceEffector2D".to_string(),
            220 => "BuoyancyEffector2D".to_string(),
            221 => "RelativeJoint2D".to_string(),
            222 => "FixedJoint2D".to_string(),
            223 => "FrictionJoint2D".to_string(),
            224 => "TargetJoint2D".to_string(),
            225 => "SliderJoint2D".to_string(),
            226 => "SpringJoint2D".to_string(),
            227 => "WheelJoint2D".to_string(),
            228 => "ClusterInputManager".to_string(),
            229 => "BaseVideoTexture".to_string(),
            230 => "NavMeshObstacle".to_string(),
            231 => "NavMeshAgent".to_string(),
            238 => "LightProbes".to_string(),
            240 => "LightProbeGroup".to_string(),
            241 => "BillboardAsset".to_string(),
            242 => "BillboardRenderer".to_string(),
            243 => "SpeedTreeWindAsset".to_string(),
            244 => "AnchoredJoint2D".to_string(),
            245 => "Joint2D".to_string(),
            246 => "SpringJoint2D".to_string(),
            247 => "DistanceJoint2D".to_string(),
            248 => "HingeJoint2D".to_string(),
            249 => "SliderJoint2D".to_string(),
            250 => "WheelJoint2D".to_string(),
            251 => "ClusterInputManager".to_string(),
            252 => "BaseVideoTexture".to_string(),
            253 => "NavMeshObstacle".to_string(),
            254 => "NavMeshAgent".to_string(),
            258 => "OcclusionCullingData".to_string(),
            271 => "Terrain".to_string(),
            272 => "LightmapParameters".to_string(),
            273 => "LightmapData".to_string(),
            290 => "ReflectionProbe".to_string(),
            319 => "AvatarMask".to_string(),
            320 => "PlayableDirector".to_string(),
            328 => "VideoPlayer".to_string(),
            329 => "VideoClip".to_string(),
            330 => "ParticleSystemForceField".to_string(),
            331 => "SpriteMask".to_string(),
            362 => "WorldAnchor".to_string(),
            363 => "OcclusionCullingData".to_string(),

            // Editor classes (1000+)
            1001 => "PrefabInstance".to_string(),
            1002 => "EditorExtensionImpl".to_string(),
            1003 => "AssetImporter".to_string(),
            1004 => "AssetDatabaseV1".to_string(),
            1005 => "Mesh3DSImporter".to_string(),
            1006 => "TextureImporter".to_string(),
            1007 => "ShaderImporter".to_string(),
            1008 => "ComputeShaderImporter".to_string(),
            1020 => "AudioImporter".to_string(),
            1026 => "HierarchyState".to_string(),
            1027 => "GUIDSerializer".to_string(),
            1028 => "AssetMetaData".to_string(),
            1029 => "DefaultAsset".to_string(),
            1030 => "DefaultImporter".to_string(),
            1031 => "TextScriptImporter".to_string(),
            1032 => "SceneAsset".to_string(),
            1034 => "NativeFormatImporter".to_string(),
            1035 => "MonoImporter".to_string(),
            1040 => "AssetServerCache".to_string(),
            1041 => "LibraryAssetImporter".to_string(),
            1042 => "ModelImporter".to_string(),
            1043 => "FBXImporter".to_string(),
            1044 => "TrueTypeFontImporter".to_string(),
            1045 => "MovieImporter".to_string(),
            1050 => "EditorBuildSettings".to_string(),
            1051 => "DDSImporter".to_string(),
            1052 => "InspectorExpandedState".to_string(),
            1053 => "AnnotationManager".to_string(),
            1054 => "PluginImporter".to_string(),
            1055 => "EditorUserBuildSettings".to_string(),
            1056 => "PVRImporter".to_string(),
            1057 => "ASTCImporter".to_string(),
            1058 => "KTXImporter".to_string(),
            1101 => "AnimatorStateTransition".to_string(),
            1102 => "AnimatorState".to_string(),
            1107 => "HumanTemplate".to_string(),
            1108 => "AnimatorStateMachine".to_string(),
            1109 => "PreviewAssetType".to_string(),
            1110 => "AnimatorTransition".to_string(),
            1111 => "SpeedTreeImporter".to_string(),
            1112 => "AnimatorTransitionBase".to_string(),
            1113 => "SubstanceImporter".to_string(),
            1114 => "LightmapParameters".to_string(),
            1115 => "LightmapSnapshot".to_string(),
            1120 => "SketchUpImporter".to_string(),
            1124 => "BuildReport".to_string(),
            1125 => "PackedAssets".to_string(),
            1126 => "VideoClipImporter".to_string(),

            // Special large IDs
            19719996 => "TilemapCollider2D".to_string(),
            41386430 => "AssetImporterLog".to_string(),
            73398921 => "VFXRenderer".to_string(),
            76251197 => "SerializableManagedRefTestClass".to_string(),
            156049354 => "Grid".to_string(),
            156483287 => "ScenesUsingAssets".to_string(),
            171741748 => "ArticulationBody".to_string(),
            181963792 => "Preset".to_string(),
            277625683 => "EmptyObject".to_string(),
            285090594 => "IConstraint".to_string(),
            687078895 => "SpriteAtlas".to_string(),

            // Unknown class ID
            _ => format!("UnityClass_{}", class_id),
        }
    }
}

impl Default for SerdeUnityLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serde_loader_creation() {
        let _loader = SerdeUnityLoader::new();
        // Just test creation
    }

    #[test]
    fn test_load_simple_yaml() {
        let loader = SerdeUnityLoader::new();
        let yaml = r#"
test_key: test_value
number: 42
boolean: true
"#;

        let result = loader.load_from_str(yaml);
        assert!(result.is_ok());

        let classes = result.unwrap();
        assert!(!classes.is_empty());
    }

    #[test]
    fn test_load_unity_gameobject() {
        let loader = SerdeUnityLoader::new();
        let yaml = r#"
GameObject:
  m_Name: Player
  m_IsActive: 1
"#;

        let result = loader.load_from_str(yaml);
        assert!(result.is_ok());

        let classes = result.unwrap();
        assert_eq!(classes.len(), 1);

        let class = &classes[0];
        if let Some(UnityValue::String(name)) = class.get("m_Name") {
            assert_eq!(name, "Player");
        }
    }
}
