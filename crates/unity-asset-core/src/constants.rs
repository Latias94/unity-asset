//! Constants and type definitions for Unity YAML parsing
//!
//! This module contains Unity-specific constants, tags, and type definitions
//! that are used throughout the parsing process.

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

/// Canonical Unity class ID-to-name catalog.
///
/// The data was clean-room synchronized from UnityPy 1.25.2 commit
/// `5567c5eddc9dbeaef27b5113f5927226bee4f8ca` (`ClassIDType.py`, MIT), whose
/// source SHA-256 is
/// `d6bd0bc9ed81ad1eca7f00132fe781747ba4df1e52534c613c0bc6f3dafede88`.
/// The `UnknownType = -1` sentinel is deliberately excluded; unknown IDs remain unknown.
const UNITY_CLASS_ID_CATALOG: &[(i32, &str)] = &[
    (0, "Object"),
    (1, "GameObject"),
    (2, "Component"),
    (3, "LevelGameManager"),
    (4, "Transform"),
    (5, "TimeManager"),
    (6, "GlobalGameManager"),
    (8, "Behaviour"),
    (9, "GameManager"),
    (11, "AudioManager"),
    (12, "ParticleAnimator"),
    (13, "InputManager"),
    (15, "EllipsoidParticleEmitter"),
    (17, "Pipeline"),
    (18, "EditorExtension"),
    (19, "Physics2DSettings"),
    (20, "Camera"),
    (21, "Material"),
    (23, "MeshRenderer"),
    (25, "Renderer"),
    (26, "ParticleRenderer"),
    (27, "Texture"),
    (28, "Texture2D"),
    (29, "OcclusionCullingSettings"),
    (30, "GraphicsSettings"),
    (33, "MeshFilter"),
    (41, "OcclusionPortal"),
    (43, "Mesh"),
    (45, "Skybox"),
    (47, "QualitySettings"),
    (48, "Shader"),
    (49, "TextAsset"),
    (50, "Rigidbody2D"),
    (51, "Physics2DManager"),
    (53, "Collider2D"),
    (54, "Rigidbody"),
    (55, "PhysicsManager"),
    (56, "Collider"),
    (57, "Joint"),
    (58, "CircleCollider2D"),
    (59, "HingeJoint"),
    (60, "PolygonCollider2D"),
    (61, "BoxCollider2D"),
    (62, "PhysicsMaterial2D"),
    (64, "MeshCollider"),
    (65, "BoxCollider"),
    (66, "CompositeCollider2D"),
    (68, "EdgeCollider2D"),
    (70, "CapsuleCollider2D"),
    (72, "ComputeShader"),
    (74, "AnimationClip"),
    (75, "ConstantForce"),
    (76, "WorldParticleCollider"),
    (78, "TagManager"),
    (81, "AudioListener"),
    (82, "AudioSource"),
    (83, "AudioClip"),
    (84, "RenderTexture"),
    (86, "CustomRenderTexture"),
    (87, "MeshParticleEmitter"),
    (88, "ParticleEmitter"),
    (89, "Cubemap"),
    (90, "Avatar"),
    (91, "AnimatorController"),
    (92, "GUILayer"),
    (93, "RuntimeAnimatorController"),
    (94, "ScriptMapper"),
    (95, "Animator"),
    (96, "TrailRenderer"),
    (98, "DelayedCallManager"),
    (102, "TextMesh"),
    (104, "RenderSettings"),
    (108, "Light"),
    (109, "CGProgram"),
    (110, "BaseAnimationTrack"),
    (111, "Animation"),
    (114, "MonoBehaviour"),
    (115, "MonoScript"),
    (116, "MonoManager"),
    (117, "Texture3D"),
    (118, "NewAnimationTrack"),
    (119, "Projector"),
    (120, "LineRenderer"),
    (121, "Flare"),
    (122, "Halo"),
    (123, "LensFlare"),
    (124, "FlareLayer"),
    (125, "HaloLayer"),
    (126, "NavMeshProjectSettings"),
    (127, "HaloManager"),
    (128, "Font"),
    (129, "PlayerSettings"),
    (130, "NamedObject"),
    (131, "GUITexture"),
    (132, "GUIText"),
    (133, "GUIElement"),
    (134, "PhysicMaterial"),
    (135, "SphereCollider"),
    (136, "CapsuleCollider"),
    (137, "SkinnedMeshRenderer"),
    (138, "FixedJoint"),
    (140, "RaycastCollider"),
    (141, "BuildSettings"),
    (142, "AssetBundle"),
    (143, "CharacterController"),
    (144, "CharacterJoint"),
    (145, "SpringJoint"),
    (146, "WheelCollider"),
    (147, "ResourceManager"),
    (148, "NetworkView"),
    (149, "NetworkManager"),
    (150, "PreloadData"),
    (152, "MovieTexture"),
    (153, "ConfigurableJoint"),
    (154, "TerrainCollider"),
    (155, "MasterServerInterface"),
    (156, "TerrainData"),
    (157, "LightmapSettings"),
    (158, "WebCamTexture"),
    (159, "EditorSettings"),
    (160, "InteractiveCloth"),
    (161, "ClothRenderer"),
    (162, "EditorUserSettings"),
    (163, "SkinnedCloth"),
    (164, "AudioReverbFilter"),
    (165, "AudioHighPassFilter"),
    (166, "AudioChorusFilter"),
    (167, "AudioReverbZone"),
    (168, "AudioEchoFilter"),
    (169, "AudioLowPassFilter"),
    (170, "AudioDistortionFilter"),
    (171, "SparseTexture"),
    (180, "AudioBehaviour"),
    (181, "AudioFilter"),
    (182, "WindZone"),
    (183, "Cloth"),
    (184, "SubstanceArchive"),
    (185, "ProceduralMaterial"),
    (186, "ProceduralTexture"),
    (187, "Texture2DArray"),
    (188, "CubemapArray"),
    (191, "OffMeshLink"),
    (192, "OcclusionArea"),
    (193, "Tree"),
    (194, "NavMeshObsolete"),
    (195, "NavMeshAgent"),
    (196, "NavMeshSettings"),
    (197, "LightProbesLegacy"),
    (198, "ParticleSystem"),
    (199, "ParticleSystemRenderer"),
    (200, "ShaderVariantCollection"),
    (205, "LODGroup"),
    (206, "BlendTree"),
    (207, "Motion"),
    (208, "NavMeshObstacle"),
    (210, "SortingGroup"),
    (212, "SpriteRenderer"),
    (213, "Sprite"),
    (214, "CachedSpriteAtlas"),
    (215, "ReflectionProbe"),
    (216, "ReflectionProbes"),
    (218, "Terrain"),
    (220, "LightProbeGroup"),
    (221, "AnimatorOverrideController"),
    (222, "CanvasRenderer"),
    (223, "Canvas"),
    (224, "RectTransform"),
    (225, "CanvasGroup"),
    (226, "BillboardAsset"),
    (227, "BillboardRenderer"),
    (228, "SpeedTreeWindAsset"),
    (229, "AnchoredJoint2D"),
    (230, "Joint2D"),
    (231, "SpringJoint2D"),
    (232, "DistanceJoint2D"),
    (233, "HingeJoint2D"),
    (234, "SliderJoint2D"),
    (235, "WheelJoint2D"),
    (236, "ClusterInputManager"),
    (237, "BaseVideoTexture"),
    (238, "NavMeshData"),
    (240, "AudioMixer"),
    (241, "AudioMixerController"),
    (243, "AudioMixerGroupController"),
    (244, "AudioMixerEffectController"),
    (245, "AudioMixerSnapshotController"),
    (246, "PhysicsUpdateBehaviour2D"),
    (247, "ConstantForce2D"),
    (248, "Effector2D"),
    (249, "AreaEffector2D"),
    (250, "PointEffector2D"),
    (251, "PlatformEffector2D"),
    (252, "SurfaceEffector2D"),
    (253, "BuoyancyEffector2D"),
    (254, "RelativeJoint2D"),
    (255, "FixedJoint2D"),
    (256, "FrictionJoint2D"),
    (257, "TargetJoint2D"),
    (258, "LightProbes"),
    (259, "LightProbeProxyVolume"),
    (271, "SampleClip"),
    (272, "AudioMixerSnapshot"),
    (273, "AudioMixerGroup"),
    (280, "NScreenBridge"),
    (290, "AssetBundleManifest"),
    (292, "UnityAdsManager"),
    (300, "RuntimeInitializeOnLoadManager"),
    (301, "CloudWebServicesManager"),
    (303, "UnityAnalyticsManager"),
    (304, "CrashReportManager"),
    (305, "PerformanceReportingManager"),
    (310, "UnityConnectSettings"),
    (319, "AvatarMask"),
    (320, "PlayableDirector"),
    (328, "VideoPlayer"),
    (329, "VideoClip"),
    (330, "ParticleSystemForceField"),
    (331, "SpriteMask"),
    (362, "WorldAnchor"),
    (363, "OcclusionCullingData"),
    (1000, "SmallestEditorClassID"),
    (1001, "PrefabInstance"),
    (1002, "EditorExtensionImpl"),
    (1003, "AssetImporter"),
    (1004, "AssetDatabaseV1"),
    (1005, "Mesh3DSImporter"),
    (1006, "TextureImporter"),
    (1007, "ShaderImporter"),
    (1008, "ComputeShaderImporter"),
    (1020, "AudioImporter"),
    (1026, "HierarchyState"),
    (1027, "GUIDSerializer"),
    (1028, "AssetMetaData"),
    (1029, "DefaultAsset"),
    (1030, "DefaultImporter"),
    (1031, "TextScriptImporter"),
    (1032, "SceneAsset"),
    (1034, "NativeFormatImporter"),
    (1035, "MonoImporter"),
    (1037, "AssetServerCache"),
    (1038, "LibraryAssetImporter"),
    (1040, "ModelImporter"),
    (1041, "FBXImporter"),
    (1042, "TrueTypeFontImporter"),
    (1044, "MovieImporter"),
    (1045, "EditorBuildSettings"),
    (1046, "DDSImporter"),
    (1048, "InspectorExpandedState"),
    (1049, "AnnotationManager"),
    (1050, "PluginImporter"),
    (1051, "EditorUserBuildSettings"),
    (1052, "PVRImporter"),
    (1053, "ASTCImporter"),
    (1054, "KTXImporter"),
    (1055, "IHVImageFormatImporter"),
    (1101, "AnimatorStateTransition"),
    (1102, "AnimatorState"),
    (1105, "HumanTemplate"),
    (1107, "AnimatorStateMachine"),
    (1108, "PreviewAnimationClip"),
    (1109, "AnimatorTransition"),
    (1110, "SpeedTreeImporter"),
    (1111, "AnimatorTransitionBase"),
    (1112, "SubstanceImporter"),
    (1113, "LightmapParameters"),
    (1120, "LightingDataAsset"),
    (1121, "GISRaster"),
    (1122, "GISRasterImporter"),
    (1123, "CadImporter"),
    (1124, "SketchUpImporter"),
    (1125, "BuildReport"),
    (1126, "PackedAssets"),
    (1127, "VideoClipImporter"),
    (2000, "ActivationLogComponent"),
    (100000, "int"),
    (100001, "bool"),
    (100002, "float"),
    (100003, "MonoObject"),
    (100004, "Collision"),
    (100005, "Vector3f"),
    (100006, "RootMotionData"),
    (100007, "Collision2D"),
    (100008, "AudioMixerLiveUpdateFloat"),
    (100009, "AudioMixerLiveUpdateBool"),
    (100010, "Polygon2D"),
    (100011, "void"),
    (19719996, "TilemapCollider2D"),
    (41386430, "AssetImporterLog"),
    (73398921, "VFXRenderer"),
    (76251197, "SerializableManagedRefTestClass"),
    (156049354, "Grid"),
    (156483287, "ScenesUsingAssets"),
    (171741748, "ArticulationBody"),
    (181963792, "Preset"),
    (277625683, "EmptyObject"),
    (285090594, "IConstraint"),
    (293259124, "TestObjectWithSpecialLayoutOne"),
    (294290339, "AssemblyDefinitionReferenceImporter"),
    (334799969, "SiblingDerived"),
    (
        342846651,
        "TestObjectWithSerializedMapStringNonAlignedStruct",
    ),
    (367388927, "SubDerived"),
    (369655926, "AssetImportInProgressProxy"),
    (382020655, "PluginBuildInfo"),
    (426301858, "EditorProjectAccess"),
    (468431735, "PrefabImporter"),
    (478637458, "TestObjectWithSerializedArray"),
    (478637459, "TestObjectWithSerializedAnimationCurve"),
    (483693784, "TilemapRenderer"),
    (488575907, "ScriptableCamera"),
    (612988286, "SpriteAtlasAsset"),
    (638013454, "SpriteAtlasDatabase"),
    (641289076, "AudioBuildInfo"),
    (644342135, "CachedSpriteAtlasRuntimeData"),
    (646504946, "RendererFake"),
    (662584278, "AssemblyDefinitionReferenceAsset"),
    (668709126, "BuiltAssetBundleInfoSet"),
    (687078895, "SpriteAtlas"),
    (747330370, "RayTracingShaderImporter"),
    (825902497, "RayTracingShader"),
    (850595691, "LightingSettings"),
    (877146078, "PlatformModuleSetup"),
    (890905787, "VersionControlSettings"),
    (895512359, "AimConstraint"),
    (937362698, "VFXManager"),
    (994735392, "VisualEffectSubgraph"),
    (994735403, "VisualEffectSubgraphOperator"),
    (994735404, "VisualEffectSubgraphBlock"),
    (1027052791, "LocalizationImporter"),
    (1091556383, "Derived"),
    (1111377672, "PropertyModificationsTargetTestObject"),
    (1114811875, "ReferencesArtifactGenerator"),
    (1152215463, "AssemblyDefinitionAsset"),
    (1154873562, "SceneVisibilityState"),
    (1183024399, "LookAtConstraint"),
    (1210832254, "SpriteAtlasImporter"),
    (1223240404, "MultiArtifactTestImporter"),
    (1268269756, "GameObjectRecorder"),
    (1325145578, "LightingDataAssetParent"),
    (1386491679, "PresetManager"),
    (1392443030, "TestObjectWithSpecialLayoutTwo"),
    (1403656975, "StreamingManager"),
    (1480428607, "LowerResBlitTexture"),
    (1542919678, "StreamingController"),
    (1571458007, "RenderPassAttachment"),
    (1628831178, "TestObjectVectorPairStringBool"),
    (1742807556, "GridLayout"),
    (1766753193, "AssemblyDefinitionImporter"),
    (1773428102, "ParentConstraint"),
    (1803986026, "FakeComponent"),
    (1818360608, "PositionConstraint"),
    (1818360609, "RotationConstraint"),
    (1818360610, "ScaleConstraint"),
    (1839735485, "Tilemap"),
    (1896753125, "PackageManifest"),
    (1896753126, "PackageManifestImporter"),
    (1953259897, "TerrainLayer"),
    (1971053207, "SpriteShapeRenderer"),
    (1977754360, "NativeObjectType"),
    (1981279845, "TestObjectWithSerializedMapStringBool"),
    (1995898324, "SerializableManagedHost"),
    (2058629509, "VisualEffectAsset"),
    (2058629510, "VisualEffectImporter"),
    (2058629511, "VisualEffectResource"),
    (2059678085, "VisualEffectObject"),
    (2083052967, "VisualEffect"),
    (2083778819, "LocalizationAsset"),
    (2089858483, "ScriptedImporter"),
];

pub(crate) fn class_id_name(class_id: i32) -> Option<&'static str> {
    let index = UNITY_CLASS_ID_CATALOG
        .binary_search_by_key(&class_id, |(known_id, _)| *known_id)
        .ok()?;
    Some(UNITY_CLASS_ID_CATALOG[index].1)
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
    pub const RECT_TRANSFORM: i32 = 224;
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
    pub const AUDIO_CLIP: &str = "AudioClip";
    pub const MESH_RENDERER: &str = "MeshRenderer";
    pub const TEXTURE_2D: &str = "Texture2D";
    pub const MESH: &str = "Mesh";
    pub const SHADER: &str = "Shader";
    pub const TEXTURE: &str = "Texture";
    pub const SPRITE: &str = "Sprite";
    pub const RECT_TRANSFORM: &str = "RectTransform";
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

        assert_eq!(class_id_name(class_ids::SPRITE), Some(class_names::SPRITE));
        assert_eq!(
            class_id_name(class_ids::SPRITE_RENDERER),
            Some("SpriteRenderer")
        );

        // Defensive: avoid "guess" mappings for unknown IDs.
        assert_eq!(class_id_name(-1), None);
        assert_eq!(class_id_name(512), None);
        assert_eq!(class_id_name(768), None);
        assert_eq!(class_id_name(i32::MAX), None);
    }

    #[test]
    fn class_catalog_is_complete_and_strictly_ordered() {
        assert_eq!(UNITY_CLASS_ID_CATALOG.len(), 367);
        assert!(
            UNITY_CLASS_ID_CATALOG
                .iter()
                .all(|(_, name)| !name.is_empty())
        );
        assert!(
            UNITY_CLASS_ID_CATALOG
                .windows(2)
                .all(|entries| entries[0].0 < entries[1].0)
        );
    }

    #[test]
    fn class_catalog_uses_canonical_rendering_names() {
        assert_eq!(class_id_name(84), Some("RenderTexture"));
        assert_eq!(class_id_name(89), Some("Cubemap"));
        assert_eq!(class_id_name(184), Some("SubstanceArchive"));
        assert_eq!(class_id_name(188), Some("CubemapArray"));
        assert_eq!(class_id_name(687_078_895), Some("SpriteAtlas"));
    }
}
