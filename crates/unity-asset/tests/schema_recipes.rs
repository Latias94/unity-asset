use std::fs;
use std::path::PathBuf;

use unity_asset::schema::{
    AudioClipResourceRecipe, DeclaredUnityVersion, HierarchyDestinationV1, HierarchyIntentV1,
    HierarchyPlacementV1, HierarchyRecipe, MaterialRecipe, MaterialTextureChange,
    PersistentArgument, PersistentCall, PersistentCallShape, PersistentCallState,
    RecipeApplicabilityStatus, RecipeError, RecipeId, RecipeLowering, RecipeRejectionCode,
    RectTransformChange, SchemaOrigin, SchemaRecipePlanner, SchemaVariantId, TransformChange,
    TransformRecipe, UnityEventEdit, UnityEventRecipe, Vector2, Vector3, recipe_capabilities,
};
use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationPlanBuilder,
    MutationPlanBuilderError, MutationPlanFragment, MutationValue, MutationValueRef, PlanPayload,
    PrepareOptions, ReferenceTarget, SequenceMutation,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, FieldPath, ObjectAddress, SourceId, SourceLocator,
    UnityValue, WorkspaceRevision,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};

#[path = "support/source_replacement.rs"]
mod source_replacement;

const MATERIAL_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!21 &2100000
Material:
  m_SavedProperties:
    m_TexEnvs:
    - first: _MainTex
      second:
        m_Texture: {fileID: 0}
        m_Scale: {x: 1, y: 1}
        m_Offset: {x: 0, y: 0}
--- !u!21 &2100001
Material:
  m_SavedProperties:
    m_TexEnvs:
    - first: {name: _MainTex}
      second:
        m_Texture: {fileID: 0}
        m_Scale: {x: 1, y: 1}
        m_Offset: {x: 0, y: 0}
--- !u!21 &2100002
Material:
  m_SavedProperties:
    m_TexEnvs:
    - first: _MainTex
      second:
        m_Texture: {fileID: 0}
        m_Scale: {x: 1, y: 1}
        m_Offset: {x: 0, y: 0}
    - first: _MainTex
      second:
        m_Texture: {fileID: 0}
        m_Scale: {x: 1, y: 1}
        m_Offset: {x: 0, y: 0}
--- !u!21 &2100003
Material:
  m_SavedProperties:
    m_TexEnvs:
    - _MainTex:
        m_Texture: {fileID: 0}
        m_Scale: {x: 1, y: 1}
        m_Offset: {x: 0, y: 0}
--- !u!21 &2100004
Material:
  m_SavedProperties:
    m_TexEnvs:
      data:
        first: {name: _MainTex}
        second:
          m_Texture: {fileID: 0}
          m_Scale: {x: 1, y: 1}
          m_Offset: {x: 0, y: 0}
"#;

const EVENT_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &11400000
MonoBehaviour:
  m_OnClick:
    m_PersistentCalls:
      m_Calls:
      - m_Target: {fileID: 100000}
        m_TargetAssemblyTypeName: Example.Target, Example
        m_MethodName: Existing
        m_Mode: 1
        m_Arguments:
          m_ObjectArgument: {fileID: 0}
          m_ObjectArgumentAssemblyTypeName: ''
          m_IntArgument: 0
          m_FloatArgument: 0
          m_StringArgument: ''
          m_BoolArgument: 0
        m_CallState: 2
--- !u!1 &100000
GameObject:
  m_Name: Target
--- !u!114 &11400001
MonoBehaviour:
  m_OnClick:
    m_PersistentCalls:
      m_Calls: []
--- !u!114 &11400002
MonoBehaviour:
  m_OnClick:
    m_PersistentCalls:
      m_Calls:
      - m_Target: {fileID: 100000}
        m_TargetAssemblyTypeName: Example.Target, Example
        m_MethodName: Broken
        m_Mode: 1
        m_Arguments:
          m_ObjectArgument: {fileID: 0}
          m_ObjectArgumentAssemblyTypeName: ''
          m_IntArgument: 0
          m_FloatArgument: 0
          m_StringArgument: ''
        m_CallState: 2
"#;

const HIERARCHY_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!4 &1
Transform:
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 2}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!4 &2
Transform:
  m_Father: {fileID: 1}
  m_Children: []
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!4 &3
Transform:
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!224 &4
RectTransform:
  m_AnchoredPosition: {x: 0, y: 0}
  m_SizeDelta: {x: 100, y: 50}
  m_AnchorMin: {x: 0, y: 0}
  m_AnchorMax: {x: 1, y: 1}
  m_Pivot: {x: 0.5, y: 0.5}
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!224 &5
RectTransform:
  m_Position: {x: 0, y: 0}
  m_SizeDelta: {x: 100, y: 50}
  m_AnchorMin: {x: 0, y: 0}
  m_AnchorMax: {x: 1, y: 1}
  m_Pivot: {x: 0.5, y: 0.5}
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!4 &6
Transform:
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 99}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!4 &7
Transform:
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 8, guid: 0123456789abcdef0123456789abcdef, type: 2}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
"#;

const ORDERED_HIERARCHY_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!4 &1
Transform:
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 2}
  - {fileID: 3}
  - {fileID: 4}
--- !u!4 &2
Transform:
  m_Father: {fileID: 1}
  m_Children: []
--- !u!4 &3
Transform:
  m_Father: {fileID: 1}
  m_Children: []
--- !u!4 &4
Transform:
  m_Father: {fileID: 1}
  m_Children: []
"#;

const RESOURCE_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!83 &8300000
AudioClip:
  m_Resource: {m_Source: archive:/CAB-a/CAB-a.resS, m_Offset: 0, m_Size: 4}
--- !u!83 &8300001
AudioClip:
  m_StreamData: {path: archive:/CAB-a/CAB-a.resS, offset: 0, size: 4}
--- !u!83 &8300002
AudioClip:
  m_Resource: {m_Source: archive:/CAB-a/CAB-a.resS, m_Offset: 0, m_Size: 4}
  m_StreamData: {path: archive:/CAB-a/CAB-a.resS, offset: 0, size: 4}
--- !u!83 &8300003
AudioClip:
  m_AudioData: T2dnUw==
--- !u!83 &8300004
AudioClip:
  m_Resource: bad-shape
  m_StreamData: {path: archive:/CAB-a/CAB-a.resS, offset: 0, size: 4}
--- !u!21 &8300005
Material:
  m_Resource: {m_Source: archive:/CAB-a/CAB-a.resS, m_Offset: 0, m_Size: 4}
--- !u!83 &8300006
AudioClip:
  m_Resource: {m_Source: archive:/CAB-a/CAB-a.resS, m_Offset: 0, m_Size: 4}
  m_StreamData: bad-shape
--- !u!83 &8300007
AudioClip:
  m_Resource: {m_Source: archive:/CAB-a/CAB-a.resS, m_Offset: -1, m_Size: 4}
--- !u!83 &8300008
AudioClip:
  m_StreamData: {path: archive:/CAB-a/CAB-a.resS, offset: 0, size: 4.5}
--- !u!83 &8300009
AudioClip:
  m_Resource: {m_Source: archive:/CAB-a/CAB-a.resS, m_Offset: 0, m_Size: 0}
--- !u!83 &8300010
AudioClip:
  m_Resource: {m_Source: archive:/CAB-a/CAB-a.resS, m_Offset: 18446744073709551615, m_Size: 1}
--- !u!83 &8300011
AudioClip:
  m_Resource: {m_Source: "", m_Offset: 0, m_Size: 4}
"#;

struct Fixture {
    _directory: tempfile::TempDir,
    workspace: AssetWorkspace,
    alias: String,
    source: SourceId,
}

impl Fixture {
    fn open(alias: &str, yaml: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(alias);
        fs::write(&path, yaml).unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        let source = workspace
            .load_path(&path, &mut AssetLoadBudget::default())
            .unwrap();
        Self {
            _directory: directory,
            workspace,
            alias: alias.to_owned(),
            source,
        }
    }

    fn address(&self, anchor: &str) -> ObjectAddress {
        ObjectAddress::yaml(
            SourceLocator::path(&self.alias).unwrap(),
            anchor.parse().unwrap(),
        )
        .unwrap()
    }
}

fn changed(lowering: RecipeLowering) -> unity_asset::workspace::MutationPlanFragment {
    lowering
        .into_fragment()
        .expect("recipe should produce a changed fragment")
}

fn assert_detach_to_root_semantics(
    fragment: &MutationPlanFragment,
    child: &ObjectAddress,
    parent: &ObjectAddress,
) {
    assert_eq!(fragment.actions().len(), 2);
    assert!(matches!(
        &fragment.actions()[0],
        GenericMutation::ReferenceReplace {
            target,
            path,
            expected,
            replacement,
            ..
        } if target == child
            && path.to_string() == "$.m_Father"
            && expected == &ReferenceTarget::object(parent.clone())
            && replacement == &ReferenceTarget::null()
    ));
    assert!(matches!(
        &fragment.actions()[1],
        GenericMutation::SequenceEdit {
            target,
            path,
            edit: SequenceMutation::Remove { index: 0 },
            ..
        } if target == parent && path.to_string() == "$.m_Children"
    ));
}

fn field_names(value: &unity_asset::workspace::MutationValue) -> Vec<&str> {
    let MutationValueRef::Object(fields) = value.view() else {
        panic!("expected object value");
    };
    fields.iter().map(|field| field.name()).collect()
}

fn binary_local_reference(value: &UnityValue) -> Option<Option<i64>> {
    let fields = value.as_object()?;
    let file_id = fields
        .get("m_FileID")
        .or_else(|| fields.get("fileID"))?
        .as_i64()?;
    let path_id = fields
        .get("m_PathID")
        .or_else(|| fields.get("pathID"))?
        .as_i64()?;
    if file_id != 0 {
        return None;
    }
    Some((path_id != 0).then_some(path_id))
}

fn binary_local_children(value: &UnityValue) -> Option<Vec<i64>> {
    value
        .as_array()?
        .iter()
        .map(|value| binary_local_reference(value).flatten())
        .collect()
}

#[test]
fn capability_catalog_and_target_applicability_are_structured() {
    let catalog = recipe_capabilities();
    assert_eq!(catalog.version(), 1);
    assert_eq!(catalog.recipes().len(), 6);
    let encoded = serde_json::to_value(catalog).unwrap();
    assert_eq!(encoded["version"], 1);
    assert_eq!(encoded["recipes"][0]["id"], "reference_retarget_v1");

    let fixture = Fixture::open("materials.prefab", MATERIAL_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let material = planner
        .inspect(&fixture.address("2100000"), &mut AssetLoadBudget::default())
        .unwrap();
    let applicability = planner
        .capabilities_for(&material, &mut AssetLoadBudget::default())
        .unwrap();
    let material = applicability
        .iter()
        .find(|entry| entry.recipe() == RecipeId::MaterialTextureEnvironmentV1)
        .unwrap();
    assert_eq!(material.status(), RecipeApplicabilityStatus::Applicable);
    assert!(material.rejection().is_none());
}

#[test]
fn generic_field_replace_rejects_semantic_owners_and_preserves_path_errors() {
    let hierarchy = Fixture::open("protected-hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = hierarchy.workspace.snapshot();
    let hierarchy_revision = snapshot.revision();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let child_address = hierarchy.address("2");
    let child = planner
        .inspect(&child_address, &mut AssetLoadBudget::default())
        .unwrap();

    let father_path = FieldPath::root().push_field("m_Father").unwrap();
    let father_leaf = father_path.clone().push_field("fileID").unwrap();
    let error = planner
        .lower_field_replace(
            &child,
            father_leaf.clone(),
            MutationValue::signed(99),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        &error,
        RecipeError::ProtectedSemanticField {
            owner: "unity-reference",
            ..
        }
    ));
    assert_eq!(
        error.code(),
        Some(RecipeRejectionCode::ProtectedSemanticField)
    );

    let children = FieldPath::root().push_field("m_Children").unwrap();
    let error = planner
        .lower_field_replace(
            &child,
            children,
            MutationValue::array(Vec::new()).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        RecipeError::ProtectedSemanticField {
            owner: "transform-hierarchy",
            ..
        }
    ));

    let invalid = father_path.clone().push_index(0).unwrap();
    let error = planner
        .lower_field_replace(
            &child,
            invalid,
            MutationValue::signed(1),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(&error, RecipeError::InvalidFieldPath { .. }));
    assert_eq!(error.code(), Some(RecipeRejectionCode::InvalidFieldPath));

    let not_a_reference = FieldPath::root().push_field("m_LocalPosition").unwrap();
    let error = planner
        .lower_reference(
            &child,
            not_a_reference,
            ReferenceTarget::null(),
            ReferenceTarget::null(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert_eq!(error.code(), Some(RecipeRejectionCode::InvalidReference));

    let resource = Fixture::open("protected-resource.asset", RESOURCE_YAML);
    let snapshot = resource.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let audio = planner
        .inspect(
            &resource.address("8300000"),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let resource_size = FieldPath::root()
        .push_field("m_Resource")
        .unwrap()
        .push_field("m_Size")
        .unwrap();
    let error = planner
        .lower_field_replace(
            &audio,
            resource_size,
            MutationValue::signed(8),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        RecipeError::ProtectedSemanticField {
            owner: "streamed-resource",
            ..
        }
    ));

    let value = child.class().value_at_path(&father_leaf).unwrap();
    let mut guard_budget = AssetLoadBudget::default();
    let guard = FieldGuard::new(
        yaml_field_schema_digest(child.class(), &father_leaf, value, &mut guard_budget).unwrap(),
        semantic_value_digest(value, &mut guard_budget).unwrap(),
    );
    let plan = MutationPlan::new(
        hierarchy.workspace.workspace_id(),
        hierarchy_revision,
        vec![child.source_expectation().clone()],
        Vec::new(),
        vec![GenericMutation::FieldReplace {
            target: child_address,
            path: father_leaf,
            guard,
            replacement: MutationValue::signed(99),
        }],
    )
    .unwrap();
    let error = hierarchy
        .workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert_eq!(
        error.report().diagnostics()[0].diagnostic().code(),
        "PREPARE_PROTECTED_SEMANTIC_FIELD"
    );
}

#[test]
fn hierarchy_applicability_matches_the_lowering_variant_for_transform_kinds() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);

    for anchor in ["1", "4"] {
        let object = planner
            .inspect(&fixture.address(anchor), &mut AssetLoadBudget::default())
            .unwrap();
        let applicability = planner
            .capabilities_for(&object, &mut AssetLoadBudget::default())
            .unwrap();
        let hierarchy = applicability
            .iter()
            .find(|entry| entry.recipe() == RecipeId::HierarchyReparentV1)
            .unwrap();
        assert_eq!(hierarchy.status(), RecipeApplicabilityStatus::Applicable);
        assert_eq!(
            hierarchy.variant(),
            Some(SchemaVariantId::HierarchyLocalReferences)
        );

        let intent = hierarchy_intent(
            &planner,
            fixture.address(anchor),
            HierarchyDestinationV1::root(),
        );
        let lowering =
            HierarchyRecipe::lower(&planner, &intent, &mut AssetLoadBudget::default()).unwrap();
        assert_eq!(lowering.report().variant(), hierarchy.variant().unwrap());
    }

    let dual_rect_yaml = HIERARCHY_YAML.replacen(
        "  m_AnchoredPosition: {x: 0, y: 0}\n",
        "  m_AnchoredPosition: {x: 0, y: 0}\n  m_Position: {x: 0, y: 0}\n",
        1,
    );
    let dual_rect = Fixture::open("dual-rect.prefab", &dual_rect_yaml);
    let dual_snapshot = dual_rect.workspace.snapshot();
    let dual_planner = SchemaRecipePlanner::new(&dual_snapshot);
    let object = dual_planner
        .inspect(&dual_rect.address("4"), &mut AssetLoadBudget::default())
        .unwrap();
    let applicability = dual_planner
        .capabilities_for(&object, &mut AssetLoadBudget::default())
        .unwrap();
    let transform = applicability
        .iter()
        .find(|entry| entry.recipe() == RecipeId::TransformV1)
        .unwrap();
    let hierarchy = applicability
        .iter()
        .find(|entry| entry.recipe() == RecipeId::HierarchyReparentV1)
        .unwrap();
    assert_eq!(transform.status(), RecipeApplicabilityStatus::Rejected);
    assert_eq!(hierarchy.status(), RecipeApplicabilityStatus::Applicable);
    let intent = hierarchy_intent(
        &dual_planner,
        dual_rect.address("4"),
        HierarchyDestinationV1::root(),
    );
    assert!(
        HierarchyRecipe::lower(&dual_planner, &intent, &mut AssetLoadBudget::default())
            .unwrap()
            .fragment()
            .is_none()
    );
}

#[test]
fn hierarchy_yaml_reference_shape_matches_prepare_and_null_semantics() {
    let null_with_external_identity = HIERARCHY_YAML.replacen(
        "  m_Father: {fileID: 0}",
        "  m_Father: {fileID: 0, guid: 0123456789abcdef0123456789abcdef, type: 2}",
        1,
    );
    let fixture = Fixture::open(
        "null-external-hierarchy.prefab",
        &null_with_external_identity,
    );
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let target = planner
        .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let capability = planner
        .capabilities_for(&target, &mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .find(|entry| entry.recipe() == RecipeId::HierarchyReparentV1)
        .unwrap();
    assert_eq!(capability.status(), RecipeApplicabilityStatus::Applicable);
    let intent = hierarchy_intent(
        &planner,
        fixture.address("1"),
        HierarchyDestinationV1::root(),
    );
    assert!(
        HierarchyRecipe::lower(&planner, &intent, &mut AssetLoadBudget::default())
            .unwrap()
            .fragment()
            .is_none()
    );

    for (name, father) in [
        ("type-without-guid", "{fileID: 0, type: 2}"),
        (
            "guid-without-type",
            "{fileID: 0, guid: 0123456789abcdef0123456789abcdef}",
        ),
        ("extra-field", "{fileID: 0, unexpected: 2}"),
    ] {
        let yaml = HIERARCHY_YAML.replacen(
            "  m_Father: {fileID: 0}",
            &format!("  m_Father: {father}"),
            1,
        );
        let fixture = Fixture::open(&format!("{name}.prefab"), &yaml);
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let target = planner
            .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
            .unwrap();
        let capability = planner
            .capabilities_for(&target, &mut AssetLoadBudget::default())
            .unwrap()
            .into_iter()
            .find(|entry| entry.recipe() == RecipeId::HierarchyReparentV1)
            .unwrap();
        assert_eq!(
            capability.rejection(),
            Some(RecipeRejectionCode::InvalidReference),
            "{name} capability"
        );
        let intent = hierarchy_intent(
            &planner,
            fixture.address("1"),
            HierarchyDestinationV1::root(),
        );
        let error =
            HierarchyRecipe::lower(&planner, &intent, &mut AssetLoadBudget::default()).unwrap_err();
        assert_eq!(error.code(), capability.rejection(), "{name} lowering");
    }
}

#[test]
fn material_recipe_matches_string_and_fast_property_keys_without_creating_fields() {
    let fixture = Fixture::open("materials.prefab", MATERIAL_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let target = ReferenceTarget::object(fixture.address("2100001"));

    for anchor in ["2100000", "2100001", "2100003", "2100004"] {
        let material = planner
            .inspect(&fixture.address(anchor), &mut AssetLoadBudget::default())
            .unwrap();
        let lowering = MaterialRecipe::lower(
            &planner,
            &material,
            "_MainTex",
            MaterialTextureChange::Retarget {
                expected: ReferenceTarget::null(),
                replacement: target.clone(),
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let fragment = changed(lowering);
        assert_eq!(fragment.actions().len(), 1);
        let GenericMutation::ReferenceReplace { path, .. } = &fragment.actions()[0] else {
            panic!("expected logical reference replacement");
        };
        assert!(path.to_string().ends_with(".m_Texture"));
    }

    let material = planner
        .inspect(&fixture.address("2100000"), &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        MaterialRecipe::lower(
            &planner,
            &material,
            "_Missing",
            MaterialTextureChange::Retarget {
                expected: ReferenceTarget::null(),
                replacement: target,
            },
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::PropertyNotFound { .. })
    ));
    assert!(!material.class().properties().contains_key("_Missing"));
}

#[test]
fn unity_event_recipe_adds_the_first_call_and_rejects_partial_argument_caches() {
    let fixture = Fixture::open("events.prefab", EVENT_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let event_path = FieldPath::root().push_field("m_OnClick").unwrap();
    let call = PersistentCall::new(
        ReferenceTarget::object(fixture.address("100000")),
        "Example.Target, Example",
        "First",
        PersistentArgument::Void,
        PersistentCallState::RuntimeOnly,
    )
    .unwrap();
    let empty = planner
        .inspect(
            &fixture.address("11400001"),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let fragment = changed(
        UnityEventRecipe::lower(
            &planner,
            &empty,
            event_path.clone(),
            UnityEventEdit::Add {
                call: call.clone(),
                shape: PersistentCallShape::WithTargetAssemblyTypeName,
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    assert!(matches!(
        &fragment.actions()[0],
        GenericMutation::SequenceEdit {
            edit: SequenceMutation::Insert { index: 0, .. },
            ..
        }
    ));

    let malformed = planner
        .inspect(
            &fixture.address("11400002"),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(matches!(
        UnityEventRecipe::lower(
            &planner,
            &malformed,
            event_path,
            UnityEventEdit::Add {
                call,
                shape: PersistentCallShape::WithTargetAssemblyTypeName,
            },
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::MissingField { .. })
    ));
}

#[test]
fn material_recipe_rejects_duplicates_and_lowers_scale_offset_atomically() {
    let fixture = Fixture::open("materials.prefab", MATERIAL_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let duplicate = planner
        .inspect(&fixture.address("2100002"), &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        MaterialRecipe::lower(
            &planner,
            &duplicate,
            "_MainTex",
            MaterialTextureChange::SetScaleOffset {
                scale: Vector2::new(2.0, 3.0),
                offset: Vector2::new(0.25, 0.5),
            },
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::DuplicateProperty { occurrences: 2, .. })
    ));

    let material = planner
        .inspect(&fixture.address("2100000"), &mut AssetLoadBudget::default())
        .unwrap();
    let change = MaterialTextureChange::SetScaleOffset {
        scale: Vector2::new(2.0, 3.0),
        offset: Vector2::new(0.25, 0.5),
    };
    let first = changed(
        MaterialRecipe::lower(
            &planner,
            &material,
            "_MainTex",
            change.clone(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    let second = changed(
        MaterialRecipe::lower(
            &planner,
            &material,
            "_MainTex",
            change,
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    assert_eq!(first, second);
    assert_eq!(first.actions().len(), 2);
}

#[test]
fn unity_event_recipe_add_replace_and_clear_preserve_stable_call_order() {
    let fixture = Fixture::open("events.prefab", EVENT_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let object = planner
        .inspect(
            &fixture.address("11400000"),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let event_path = FieldPath::root().push_field("m_OnClick").unwrap();
    let added = PersistentCall::new(
        ReferenceTarget::object(fixture.address("100000")),
        "Example.Target, Example",
        "Added",
        PersistentArgument::Int(7),
        PersistentCallState::EditorAndRuntime,
    )
    .unwrap();

    let add = changed(
        UnityEventRecipe::lower(
            &planner,
            &object,
            event_path.clone(),
            UnityEventEdit::Add {
                call: added.clone(),
                shape: PersistentCallShape::WithTargetAssemblyTypeName,
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    let GenericMutation::SequenceEdit {
        edit: SequenceMutation::Insert { index, value },
        ..
    } = &add.actions()[0]
    else {
        panic!("expected a persistent-call insertion");
    };
    assert_eq!(*index, 1);
    assert_eq!(
        field_names(value),
        [
            "m_Arguments",
            "m_CallState",
            "m_MethodName",
            "m_Mode",
            "m_Target",
            "m_TargetAssemblyTypeName",
        ]
    );
    let MutationValueRef::Object(first_call) = value.view() else {
        unreachable!();
    };
    assert!(matches!(
        first_call
            .iter()
            .find(|field| field.name() == "m_Target")
            .unwrap()
            .value()
            .view(),
        MutationValueRef::Reference(_)
    ));

    let replacement = PersistentCall::new(
        ReferenceTarget::null(),
        "Example.Target, Example",
        "Replacement",
        PersistentArgument::Bool(true),
        PersistentCallState::Off,
    )
    .unwrap();
    let replace = UnityEventRecipe::lower(
        &planner,
        &object,
        event_path.clone(),
        UnityEventEdit::Replace {
            index: 0,
            call: replacement,
        },
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    assert!(matches!(
        &replace.fragment().unwrap().actions()[0],
        GenericMutation::SequenceEdit {
            edit: SequenceMutation::Replace { index: 0, .. },
            ..
        }
    ));

    let clear = changed(
        UnityEventRecipe::lower(
            &planner,
            &object,
            event_path,
            UnityEventEdit::Clear,
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    assert!(matches!(
        &clear.actions()[0],
        GenericMutation::SequenceEdit {
            edit: SequenceMutation::Clear,
            ..
        }
    ));
}

#[test]
fn unity_event_clear_is_unchanged_when_the_call_sequence_is_empty() {
    let fixture = Fixture::open("events.prefab", EVENT_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let object = planner
        .inspect(
            &fixture.address("11400001"),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let lowering = UnityEventRecipe::lower(
        &planner,
        &object,
        FieldPath::root().push_field("m_OnClick").unwrap(),
        UnityEventEdit::Clear,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    assert!(matches!(lowering, RecipeLowering::Unchanged { .. }));
    assert!(lowering.fragment().is_none());
    assert_eq!(lowering.report().operation_count(), 0);
    assert_eq!(lowering.report().payload_count(), 0);
}

#[test]
fn unity_event_recipe_rejects_out_of_bounds_edits() {
    let fixture = Fixture::open("events.prefab", EVENT_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let object = planner
        .inspect(
            &fixture.address("11400000"),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let event_path = FieldPath::root().push_field("m_OnClick").unwrap();
    let call = PersistentCall::new(
        ReferenceTarget::null(),
        "Type",
        "Call",
        PersistentArgument::Void,
        PersistentCallState::RuntimeOnly,
    )
    .unwrap();
    assert!(matches!(
        UnityEventRecipe::lower(
            &planner,
            &object,
            event_path,
            UnityEventEdit::Replace {
                index: 1,
                call: call.clone(),
            },
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::CallIndexOutOfBounds { index: 1, len: 1 })
    ));
}

#[test]
fn transform_recipe_selects_modern_and_legacy_rect_fields_and_rejects_wrong_class() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let transform = planner
        .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let fragment = changed(
        TransformRecipe::lower_transform(
            &planner,
            &transform,
            TransformChange::LocalPosition(Vector3::new(1.0, 2.0, 3.0)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    let GenericMutation::FieldReplace { path, .. } = &fragment.actions()[0] else {
        unreachable!();
    };
    assert_eq!(path.to_string(), "$.m_LocalPosition");

    for (anchor, expected) in [("4", "$.m_AnchoredPosition"), ("5", "$.m_Position")] {
        let rect = planner
            .inspect(&fixture.address(anchor), &mut AssetLoadBudget::default())
            .unwrap();
        let fragment = changed(
            TransformRecipe::lower_rect_transform(
                &planner,
                &rect,
                RectTransformChange::AnchoredPosition(Vector2::new(4.0, 5.0)),
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        );
        let GenericMutation::FieldReplace { path, .. } = &fragment.actions()[0] else {
            unreachable!();
        };
        assert_eq!(path.to_string(), expected);
    }

    let event_fixture = Fixture::open("events.prefab", EVENT_YAML);
    let event_snapshot = event_fixture.workspace.snapshot();
    let event_planner = SchemaRecipePlanner::new(&event_snapshot);
    let wrong = event_planner
        .inspect(
            &event_fixture.address("11400000"),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let error = TransformRecipe::lower_transform(
        &event_planner,
        &wrong,
        TransformChange::LocalPosition(Vector3::new(1.0, 2.0, 3.0)),
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert_eq!(error.code(), Some(RecipeRejectionCode::WrongClass));
}

fn hierarchy_intent(
    planner: &SchemaRecipePlanner<'_>,
    child: ObjectAddress,
    destination: HierarchyDestinationV1,
) -> HierarchyIntentV1 {
    HierarchyIntentV1::new(
        planner.workspace_id(),
        planner.revision(),
        child,
        destination,
    )
}

const MINIMAL_REPARENT_HIERARCHY_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!4 &1
Transform:
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 2}
--- !u!4 &2
Transform:
  m_Father: {fileID: 1}
  m_Children: []
--- !u!4 &3
Transform:
  m_Father: {fileID: 0}
  m_Children: []
"#;

fn hierarchy_with_unrelated_transforms(unrelated: usize) -> String {
    let mut yaml = String::from(MINIMAL_REPARENT_HIERARCHY_YAML);
    for index in 0..unrelated {
        let anchor = 10_000 + index;
        yaml.push_str(&format!(
            "--- !u!4 &{anchor}\nTransform:\n  m_Father: {{fileID: 0}}\n  m_Children: []\n"
        ));
    }
    yaml
}

fn hierarchy_with_unrelated_gameobjects(unrelated: usize) -> String {
    let mut yaml = String::from(MINIMAL_REPARENT_HIERARCHY_YAML);
    for index in 0..unrelated {
        let anchor = 10_000 + index;
        yaml.push_str(&format!(
            "--- !u!1 &{anchor}\nGameObject:\n  m_Name: Unrelated\n"
        ));
    }
    yaml
}

fn deep_hierarchy(depth: usize) -> String {
    let mut yaml = String::from("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n");
    for index in 1..=depth {
        let parent = index.saturating_sub(1);
        let children = if index == depth {
            "[]".to_owned()
        } else {
            format!("\n  - {{fileID: {}}}", index + 1)
        };
        yaml.push_str(&format!(
            "--- !u!4 &{index}\nTransform:\n  m_Father: {{fileID: {parent}}}\n  m_Children: {children}\n"
        ));
    }
    yaml
}

fn wide_hierarchy(width: usize) -> String {
    let mut yaml = String::from(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!4 &1\nTransform:\n  m_Father: {fileID: 0}\n  m_Children:\n",
    );
    for index in 2..=width + 1 {
        yaml.push_str(&format!("  - {{fileID: {index}}}\n"));
    }
    for index in 2..=width + 1 {
        yaml.push_str(&format!(
            "--- !u!4 &{index}\nTransform:\n  m_Father: {{fileID: 1}}\n  m_Children: []\n"
        ));
    }
    yaml
}

fn hierarchy_usage(
    yaml: &str,
    child_anchor: &str,
    parent_anchor: Option<&str>,
) -> unity_asset::AssetLoadUsage {
    let fixture = Fixture::open("large-hierarchy.prefab", yaml);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let destination = match parent_anchor {
        Some(parent) => {
            HierarchyDestinationV1::parent(fixture.address(parent), HierarchyPlacementV1::Last)
        }
        None => HierarchyDestinationV1::root(),
    };
    let intent = hierarchy_intent(&planner, fixture.address(child_anchor), destination);
    let mut budget = AssetLoadBudget::default();
    HierarchyRecipe::lower(&planner, &intent, &mut budget).unwrap();
    budget.usage()
}

fn assert_linear_entry_growth(samples: [unity_asset::AssetLoadUsage; 3]) {
    let first = samples[1].entries - samples[0].entries;
    let second = samples[2].entries - samples[1].entries;
    assert!(first > 0);
    assert!(second > first);
    assert!(
        second.saturating_mul(100) <= first.saturating_mul(205),
        "doubling input should at most double marginal work: {samples:?}"
    );
}

#[test]
fn hierarchy_projection_work_is_linear_for_sparse_deep_and_wide_sources() {
    assert_linear_entry_growth([64, 128, 256].map(|count| {
        hierarchy_usage(&hierarchy_with_unrelated_gameobjects(count), "2", Some("3"))
    }));
    assert_linear_entry_growth(
        [64, 128, 256].map(|count| {
            hierarchy_usage(&hierarchy_with_unrelated_transforms(count), "2", Some("3"))
        }),
    );
    assert_linear_entry_growth(
        [32, 64, 128]
            .map(|depth| hierarchy_usage(&deep_hierarchy(depth), &depth.to_string(), None)),
    );
    assert_linear_entry_growth(
        [32, 64, 128].map(|width| hierarchy_usage(&wide_hierarchy(width), "2", None)),
    );
}

#[test]
fn sparse_hierarchy_projection_retains_only_transform_nodes() {
    let small_count = 128_usize;
    let large_count = 256_usize;
    let small = hierarchy_usage(
        &hierarchy_with_unrelated_gameobjects(small_count),
        "2",
        Some("3"),
    );
    let large = hierarchy_usage(
        &hierarchy_with_unrelated_gameobjects(large_count),
        "2",
        Some("3"),
    );
    let marginal_bytes = large.bytes - small.bytes;
    let maximum_descriptor_bytes = u64::try_from(large_count - small_count).unwrap() * 32;
    assert!(
        marginal_bytes <= maximum_descriptor_bytes,
        "unrelated objects must not reserve hierarchy nodes: small={small:?}, large={large:?}"
    );
}

#[test]
fn hierarchy_validation_and_output_have_exact_budget_boundaries() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let intent = hierarchy_intent(
        &planner,
        fixture.address("2"),
        HierarchyDestinationV1::parent(fixture.address("3"), HierarchyPlacementV1::Last),
    );
    let run = |budget: &mut AssetLoadBudget| {
        HierarchyRecipe::lower(&planner, &intent, budget)
            .map(|_| ())
            .map_err(|error| matches!(error, RecipeError::Budget(_)))
    };
    let mut measured = AssetLoadBudget::default();
    run(&mut measured).unwrap();
    let usage = measured.usage();
    assert!(usage.entries > 1 && usage.bytes > 1);
    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    run(&mut exact).unwrap();
    assert_eq!(exact.usage(), usage);
    for limits in [
        AssetLoadLimits {
            max_entries: usage.entries - 1,
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        },
    ] {
        let mut one_short = AssetLoadBudget::new(limits).unwrap();
        assert_eq!(run(&mut one_short), Err(true));
    }
}

#[test]
fn hierarchy_recipe_emits_complete_ordered_reparent_fragment() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let intent = hierarchy_intent(
        &planner,
        fixture.address("2"),
        HierarchyDestinationV1::parent(fixture.address("3"), HierarchyPlacementV1::Last),
    );
    let lower = || {
        changed(HierarchyRecipe::lower(&planner, &intent, &mut AssetLoadBudget::default()).unwrap())
    };
    let fragment = lower();
    assert_eq!(fragment, lower());
    assert_eq!(fragment.actions().len(), 3);
    assert!(matches!(
        &fragment.actions()[0],
        GenericMutation::ReferenceReplace { path, .. } if path.to_string() == "$.m_Father"
    ));
    for action in &fragment.actions()[1..] {
        let GenericMutation::SequenceEdit { path, edit, .. } = action else {
            panic!("expected parent child-array sequence edit");
        };
        assert_eq!(path.to_string(), "$.m_Children");
        assert!(matches!(
            edit,
            SequenceMutation::Remove { .. } | SequenceMutation::Insert { .. }
        ));
    }
}

#[test]
fn hierarchy_root_and_existing_position_no_ops_do_not_emit_fragments() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);

    let root_intent = hierarchy_intent(
        &planner,
        fixture.address("2"),
        HierarchyDestinationV1::root(),
    );
    let root_fragment = changed(
        HierarchyRecipe::lower(&planner, &root_intent, &mut AssetLoadBudget::default()).unwrap(),
    );
    assert_eq!(root_fragment.actions().len(), 2);
    assert!(matches!(
        &root_fragment.actions()[0],
        GenericMutation::ReferenceReplace { replacement, .. } if replacement == &ReferenceTarget::null()
    ));

    for intent in [
        hierarchy_intent(
            &planner,
            fixture.address("1"),
            HierarchyDestinationV1::root(),
        ),
        hierarchy_intent(
            &planner,
            fixture.address("2"),
            HierarchyDestinationV1::parent(fixture.address("1"), HierarchyPlacementV1::First),
        ),
    ] {
        let lowering =
            HierarchyRecipe::lower(&planner, &intent, &mut AssetLoadBudget::default()).unwrap();
        assert!(lowering.fragment().is_none());
    }
}

#[test]
fn hierarchy_move_indices_name_first_last_and_index_final_positions() {
    let fixture = Fixture::open("ordered-hierarchy.prefab", ORDERED_HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);

    for (child, placement, expected_from, expected_to) in [
        ("4", HierarchyPlacementV1::First, 2, 0),
        ("2", HierarchyPlacementV1::Last, 0, 2),
        ("2", HierarchyPlacementV1::Index { index: 1 }, 0, 1),
    ] {
        let intent = hierarchy_intent(
            &planner,
            fixture.address(child),
            HierarchyDestinationV1::parent(fixture.address("1"), placement),
        );
        let fragment = changed(
            HierarchyRecipe::lower(&planner, &intent, &mut AssetLoadBudget::default()).unwrap(),
        );
        assert!(matches!(
            &fragment.actions()[0],
            GenericMutation::SequenceEdit {
                edit: SequenceMutation::Move { from, to },
                ..
            } if *from == expected_from && *to == expected_to
        ));
    }
}

#[test]
fn binary_and_yaml_hierarchy_detach_have_equivalent_semantics() {
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin",
    );
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&sample, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let locator = SourceLocator::path("transform_hierarchy_v22.assets.bin").unwrap();
    let parent_address = ObjectAddress::binary_direct(locator.clone(), 1).unwrap();
    let child_address = ObjectAddress::binary_direct(locator, 2).unwrap();
    let parent = planner
        .inspect(&parent_address, &mut AssetLoadBudget::default())
        .unwrap();
    let child = planner
        .inspect(&child_address, &mut AssetLoadBudget::default())
        .unwrap();

    for object in [&parent, &child] {
        assert_eq!(object.class().class_id(), 4);
        assert_eq!(object.class().class_name(), "Transform");
        assert_eq!(object.provenance().origin(), SchemaOrigin::EmbeddedTypeTree);
        assert!(object.provenance().schema_digest().is_some());
        assert!(matches!(
            object
                .provenance()
                .binary_version()
                .unwrap()
                .declared_unity(),
            DeclaredUnityVersion::Parsed { .. }
        ));
    }
    assert_eq!(
        parent
            .class()
            .get("m_Father")
            .and_then(binary_local_reference),
        Some(None)
    );
    assert_eq!(
        parent
            .class()
            .get("m_Children")
            .and_then(binary_local_children),
        Some(vec![2])
    );
    assert_eq!(
        child
            .class()
            .get("m_Father")
            .and_then(binary_local_reference),
        Some(Some(1))
    );
    assert_eq!(
        child
            .class()
            .get("m_Children")
            .and_then(binary_local_children),
        Some(Vec::new())
    );

    let intent = hierarchy_intent(
        &planner,
        child_address.clone(),
        HierarchyDestinationV1::root(),
    );
    let binary_fragment = changed(
        HierarchyRecipe::lower(&planner, &intent, &mut AssetLoadBudget::default()).unwrap(),
    );
    assert_detach_to_root_semantics(&binary_fragment, &child_address, &parent_address);
    let mut builder = MutationPlanBuilder::new(planner.workspace_id(), planner.revision());
    builder.append(binary_fragment).unwrap();
    let prepared = workspace
        .prepare(
            builder.build().unwrap(),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared_view = prepared.view();
    let prepared_planner = SchemaRecipePlanner::new(&prepared_view);
    let prepared_intent = hierarchy_intent(
        &prepared_planner,
        child_address.clone(),
        HierarchyDestinationV1::root(),
    );
    assert!(
        HierarchyRecipe::lower(
            &prepared_planner,
            &prepared_intent,
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
        .fragment()
        .is_none()
    );

    let yaml = Fixture::open("hierarchy-equivalence.prefab", HIERARCHY_YAML);
    let yaml_snapshot = yaml.workspace.snapshot();
    let yaml_planner = SchemaRecipePlanner::new(&yaml_snapshot);
    let yaml_child = yaml.address("2");
    let yaml_parent = yaml.address("1");
    let yaml_intent = hierarchy_intent(
        &yaml_planner,
        yaml_child.clone(),
        HierarchyDestinationV1::root(),
    );
    let yaml_fragment = changed(
        HierarchyRecipe::lower(&yaml_planner, &yaml_intent, &mut AssetLoadBudget::default())
            .unwrap(),
    );
    assert_detach_to_root_semantics(&yaml_fragment, &yaml_child, &yaml_parent);
}

#[test]
fn hierarchy_recipe_rejects_stale_or_foreign_intents_before_inspection() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let other = Fixture::open("other.prefab", HIERARCHY_YAML);
    let other_snapshot = other.workspace.snapshot();

    let foreign = HierarchyIntentV1::for_view(
        &other_snapshot,
        fixture.address("2"),
        HierarchyDestinationV1::root(),
    );
    let error =
        HierarchyRecipe::lower(&planner, &foreign, &mut AssetLoadBudget::default()).unwrap_err();
    assert_eq!(
        error.code(),
        Some(RecipeRejectionCode::HierarchyWorkspaceMismatch)
    );

    let stale = HierarchyIntentV1::new(
        planner.workspace_id(),
        WorkspaceRevision::new(unity_asset::DigestV1::hash_bytes(
            b"stale hierarchy revision",
        )),
        fixture.address("2"),
        HierarchyDestinationV1::root(),
    );
    let error =
        HierarchyRecipe::lower(&planner, &stale, &mut AssetLoadBudget::default()).unwrap_err();
    assert_eq!(
        error.code(),
        Some(RecipeRejectionCode::HierarchyRevisionMismatch)
    );

    let mut unloaded = Fixture::open("unloaded.prefab", HIERARCHY_YAML);
    let unloaded_address = unloaded.address("2");
    unloaded
        .workspace
        .unload_source(unloaded.source, &mut AssetLoadBudget::default())
        .unwrap();
    let unloaded_snapshot = unloaded.workspace.snapshot();
    let unloaded_planner = SchemaRecipePlanner::new(&unloaded_snapshot);
    let intent = HierarchyIntentV1::for_view(
        &unloaded_snapshot,
        unloaded_address,
        HierarchyDestinationV1::root(),
    );
    let error = HierarchyRecipe::lower(&unloaded_planner, &intent, &mut AssetLoadBudget::default())
        .unwrap_err();
    assert_eq!(error.code(), Some(RecipeRejectionCode::TargetUnloaded));
}

#[test]
fn hierarchy_recipe_rejects_self_cycles_missing_and_out_of_bounds_destinations() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);

    let self_parent = hierarchy_intent(
        &planner,
        fixture.address("2"),
        HierarchyDestinationV1::parent(fixture.address("2"), HierarchyPlacementV1::Last),
    );
    assert!(matches!(
        HierarchyRecipe::lower(&planner, &self_parent, &mut AssetLoadBudget::default()),
        Err(RecipeError::SelfParent { .. })
    ));

    let cycle = hierarchy_intent(
        &planner,
        fixture.address("1"),
        HierarchyDestinationV1::parent(fixture.address("2"), HierarchyPlacementV1::Last),
    );
    assert!(matches!(
        HierarchyRecipe::lower(&planner, &cycle, &mut AssetLoadBudget::default()),
        Err(RecipeError::HierarchyCycle { .. })
    ));

    let missing = hierarchy_intent(
        &planner,
        fixture.address("2"),
        HierarchyDestinationV1::parent(fixture.address("99"), HierarchyPlacementV1::Last),
    );
    assert!(matches!(
        HierarchyRecipe::lower(&planner, &missing, &mut AssetLoadBudget::default()),
        Err(RecipeError::MissingParent { .. })
    ));

    let out_of_bounds = hierarchy_intent(
        &planner,
        fixture.address("2"),
        HierarchyDestinationV1::parent(
            fixture.address("1"),
            HierarchyPlacementV1::Index { index: 2 },
        ),
    );
    assert!(matches!(
        HierarchyRecipe::lower(&planner, &out_of_bounds, &mut AssetLoadBudget::default()),
        Err(RecipeError::ChildPlacementOutOfBounds {
            index: 2,
            maximum: 0,
        })
    ));
}

#[test]
fn hierarchy_projection_rejects_dangling_external_asymmetric_and_duplicate_edges() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);

    let dangling = hierarchy_intent(
        &planner,
        fixture.address("6"),
        HierarchyDestinationV1::root(),
    );
    assert!(matches!(
        HierarchyRecipe::lower(&planner, &dangling, &mut AssetLoadBudget::default()),
        Err(RecipeError::MissingChild { .. })
    ));

    let external = hierarchy_intent(
        &planner,
        fixture.address("7"),
        HierarchyDestinationV1::root(),
    );
    assert!(matches!(
        HierarchyRecipe::lower(&planner, &external, &mut AssetLoadBudget::default()),
        Err(RecipeError::UnresolvedReference { .. })
    ));

    let asymmetric_yaml =
        HIERARCHY_YAML.replacen("  m_Father: {fileID: 1}", "  m_Father: {fileID: 0}", 1);
    let asymmetric = Fixture::open("asymmetric.prefab", &asymmetric_yaml);
    let asymmetric_snapshot = asymmetric.workspace.snapshot();
    let asymmetric_planner = SchemaRecipePlanner::new(&asymmetric_snapshot);
    let asymmetric_object = asymmetric_planner
        .inspect(&asymmetric.address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let hierarchy_capability = asymmetric_planner
        .capabilities_for(&asymmetric_object, &mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .find(|entry| entry.recipe() == RecipeId::HierarchyReparentV1)
        .unwrap();
    assert_eq!(
        hierarchy_capability.rejection(),
        Some(RecipeRejectionCode::ParentChildMismatch)
    );
    let intent = hierarchy_intent(
        &asymmetric_planner,
        asymmetric.address("1"),
        HierarchyDestinationV1::root(),
    );
    assert!(matches!(
        HierarchyRecipe::lower(
            &asymmetric_planner,
            &intent,
            &mut AssetLoadBudget::default()
        ),
        Err(RecipeError::ParentChildMismatch { .. })
    ));

    let duplicate_yaml = HIERARCHY_YAML.replacen(
        "  m_Children:\n  - {fileID: 2}\n  m_LocalPosition",
        "  m_Children:\n  - {fileID: 2}\n  - {fileID: 2}\n  m_LocalPosition",
        1,
    );
    let duplicate = Fixture::open("duplicate.prefab", &duplicate_yaml);
    let duplicate_snapshot = duplicate.workspace.snapshot();
    let duplicate_planner = SchemaRecipePlanner::new(&duplicate_snapshot);
    let intent = hierarchy_intent(
        &duplicate_planner,
        duplicate.address("1"),
        HierarchyDestinationV1::root(),
    );
    assert!(matches!(
        HierarchyRecipe::lower(&duplicate_planner, &intent, &mut AssetLoadBudget::default()),
        Err(RecipeError::DuplicateChildMembership { .. })
    ));

    let multiple_parent_yaml = format!(
        "{HIERARCHY_YAML}\n--- !u!4 &9\nTransform:\n  m_Father: {{fileID: 0}}\n  m_Children:\n  - {{fileID: 2}}\n"
    );
    let multiple_parent = Fixture::open("multiple-parent.prefab", &multiple_parent_yaml);
    let multiple_parent_snapshot = multiple_parent.workspace.snapshot();
    let multiple_parent_planner = SchemaRecipePlanner::new(&multiple_parent_snapshot);
    let intent = hierarchy_intent(
        &multiple_parent_planner,
        multiple_parent.address("2"),
        HierarchyDestinationV1::root(),
    );
    assert!(matches!(
        HierarchyRecipe::lower(
            &multiple_parent_planner,
            &intent,
            &mut AssetLoadBudget::default()
        ),
        Err(RecipeError::MultipleParents { .. })
    ));

    let mismatched_tag_yaml =
        format!("{HIERARCHY_YAML}\n--- !u!4 &9\nNotTransform:\n  m_Children:\n  - {{fileID: 2}}\n");
    let mismatched_tag = Fixture::open("mismatched-tag.prefab", &mismatched_tag_yaml);
    let mismatched_tag_snapshot = mismatched_tag.workspace.snapshot();
    let mismatched_tag_planner = SchemaRecipePlanner::new(&mismatched_tag_snapshot);
    let intent = hierarchy_intent(
        &mismatched_tag_planner,
        mismatched_tag.address("2"),
        HierarchyDestinationV1::root(),
    );
    assert!(
        HierarchyRecipe::lower(
            &mismatched_tag_planner,
            &intent,
            &mut AssetLoadBudget::default()
        )
        .is_ok()
    );

    let observed_sibling_yaml = HIERARCHY_YAML.replacen(
        "  m_Children:\n  - {fileID: 2}\n  m_LocalPosition",
        "  m_Children:\n  - {fileID: 2}\n  - {fileID: 9}\n  m_LocalPosition",
        1,
    ) + "\n--- !u!4 &9\nTransform:\n  m_Father: {fileID: 1}\n  m_Children:\n  - {fileID: 2}\n";
    let observed_sibling = Fixture::open("observed-sibling.prefab", &observed_sibling_yaml);
    let observed_sibling_snapshot = observed_sibling.workspace.snapshot();
    let observed_sibling_planner = SchemaRecipePlanner::new(&observed_sibling_snapshot);
    let intent = hierarchy_intent(
        &observed_sibling_planner,
        observed_sibling.address("2"),
        HierarchyDestinationV1::root(),
    );
    assert!(matches!(
        HierarchyRecipe::lower(
            &observed_sibling_planner,
            &intent,
            &mut AssetLoadBudget::default()
        ),
        Err(RecipeError::MultipleParents { .. })
    ));

    let adopted_direct_child_yaml = HIERARCHY_YAML.replacen(
        "  m_Children:\n  - {fileID: 2}\n  m_LocalPosition",
        "  m_Children:\n  - {fileID: 2}\n  - {fileID: 9}\n  m_LocalPosition",
        1,
    ) + "\n--- !u!4 &9\nTransform:\n  m_Father: {fileID: 1}\n  m_Children: []\n--- !u!4 &10\nTransform:\n  m_Father: {fileID: 0}\n  m_Children:\n  - {fileID: 9}\n";
    let adopted_direct_child =
        Fixture::open("adopted-direct-child.prefab", &adopted_direct_child_yaml);
    let adopted_direct_child_snapshot = adopted_direct_child.workspace.snapshot();
    let adopted_direct_child_planner = SchemaRecipePlanner::new(&adopted_direct_child_snapshot);
    let intent = hierarchy_intent(
        &adopted_direct_child_planner,
        adopted_direct_child.address("2"),
        HierarchyDestinationV1::root(),
    );
    assert!(matches!(
        HierarchyRecipe::lower(
            &adopted_direct_child_planner,
            &intent,
            &mut AssetLoadBudget::default()
        ),
        Err(RecipeError::MultipleParents { .. })
    ));
}

#[test]
fn hierarchy_projection_rejects_cross_source_wrong_class_and_wrong_shape() {
    let mut fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let other_path = fixture._directory.path().join("other.prefab");
    fs::write(&other_path, HIERARCHY_YAML).unwrap();
    fixture
        .workspace
        .load_path(&other_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let other_parent = ObjectAddress::yaml(
        SourceLocator::path("other.prefab").unwrap(),
        "3".parse().unwrap(),
    )
    .unwrap();
    let cross_source = hierarchy_intent(
        &planner,
        fixture.address("2"),
        HierarchyDestinationV1::parent(other_parent, HierarchyPlacementV1::Last),
    );
    assert!(matches!(
        HierarchyRecipe::lower(&planner, &cross_source, &mut AssetLoadBudget::default()),
        Err(RecipeError::CrossSourceHierarchy)
    ));

    let wrong_class_yaml =
        format!("{HIERARCHY_YAML}\n--- !u!1 &8\nGameObject:\n  m_Name: NotATransform\n");
    let wrong_class = Fixture::open("wrong-class.prefab", &wrong_class_yaml);
    let wrong_class_snapshot = wrong_class.workspace.snapshot();
    let wrong_class_planner = SchemaRecipePlanner::new(&wrong_class_snapshot);
    let intent = hierarchy_intent(
        &wrong_class_planner,
        wrong_class.address("8"),
        HierarchyDestinationV1::root(),
    );
    assert!(matches!(
        HierarchyRecipe::lower(
            &wrong_class_planner,
            &intent,
            &mut AssetLoadBudget::default()
        ),
        Err(RecipeError::WrongClass { .. })
    ));

    let wrong_shape_yaml = HIERARCHY_YAML.replacen(
        "  m_Children:\n  - {fileID: 99}\n  m_LocalPosition",
        "  m_Children: not-an-array\n  m_LocalPosition",
        1,
    );
    let wrong_shape = Fixture::open("wrong-shape.prefab", &wrong_shape_yaml);
    let wrong_shape_snapshot = wrong_shape.workspace.snapshot();
    let wrong_shape_planner = SchemaRecipePlanner::new(&wrong_shape_snapshot);
    let wrong_shape_object = wrong_shape_planner
        .inspect(&wrong_shape.address("6"), &mut AssetLoadBudget::default())
        .unwrap();
    let applicability = wrong_shape_planner
        .capabilities_for(&wrong_shape_object, &mut AssetLoadBudget::default())
        .unwrap();
    let hierarchy = applicability
        .iter()
        .find(|entry| entry.recipe() == RecipeId::HierarchyReparentV1)
        .unwrap();
    assert_eq!(hierarchy.status(), RecipeApplicabilityStatus::Rejected);
    assert_eq!(
        hierarchy.rejection(),
        Some(RecipeRejectionCode::WrongFieldShape)
    );
    let intent = hierarchy_intent(
        &wrong_shape_planner,
        wrong_shape.address("6"),
        HierarchyDestinationV1::root(),
    );
    let error = HierarchyRecipe::lower(
        &wrong_shape_planner,
        &intent,
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert_eq!(error.code(), hierarchy.rejection());
}

#[test]
fn hierarchy_lowering_reads_prepared_view_mutations() {
    let fixture = Fixture::open("prepared-hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let first_intent = hierarchy_intent(
        &planner,
        fixture.address("2"),
        HierarchyDestinationV1::parent(fixture.address("3"), HierarchyPlacementV1::Last),
    );
    let first = changed(
        HierarchyRecipe::lower(&planner, &first_intent, &mut AssetLoadBudget::default()).unwrap(),
    );
    let mut builder = MutationPlanBuilder::new(planner.workspace_id(), planner.revision());
    builder.append(first).unwrap();
    let prepared = fixture
        .workspace
        .prepare(
            builder.build().unwrap(),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let view = prepared.view();
    let prepared_planner = SchemaRecipePlanner::new(&view);
    let root_intent = hierarchy_intent(
        &prepared_planner,
        fixture.address("2"),
        HierarchyDestinationV1::root(),
    );
    let fragment = changed(
        HierarchyRecipe::lower(
            &prepared_planner,
            &root_intent,
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    assert_eq!(fragment.actions().len(), 2);
    assert!(matches!(
        &fragment.actions()[0],
        GenericMutation::ReferenceReplace { expected, replacement, .. }
            if expected == &ReferenceTarget::object(fixture.address("3"))
                && replacement == &ReferenceTarget::null()
    ));
}

#[test]
fn audio_clip_recipe_classifies_all_candidate_combinations_without_allocating_cab_state() {
    let fixture = Fixture::open("audio.prefab", RESOURCE_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);

    for (anchor, expected_path, expected_variant) in [
        (
            "8300000",
            "$.m_Resource",
            SchemaVariantId::AudioClipResource,
        ),
        (
            "8300001",
            "$.m_StreamData",
            SchemaVariantId::AudioClipStreamDataCompatibility,
        ),
        (
            "8300002",
            "$.m_Resource",
            SchemaVariantId::AudioClipResource,
        ),
        (
            "8300006",
            "$.m_Resource",
            SchemaVariantId::AudioClipResource,
        ),
    ] {
        let object = planner
            .inspect(&fixture.address(anchor), &mut AssetLoadBudget::default())
            .unwrap();
        let applicability = planner
            .capabilities_for(&object, &mut AssetLoadBudget::default())
            .unwrap();
        let resource = applicability
            .iter()
            .find(|entry| entry.recipe() == RecipeId::AudioClipStreamedResourceV1)
            .unwrap();
        let fragment = changed(
            AudioClipResourceRecipe::lower(
                &planner,
                &object,
                PlanPayload::new(b"OggS".to_vec()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        );
        assert_eq!(fragment.actions().len(), 1);
        assert_eq!(fragment.payloads().len(), 1);
        let GenericMutation::ResourceReplace { path, .. } = &fragment.actions()[0] else {
            unreachable!();
        };
        assert_eq!(path.to_string(), expected_path);
        assert_eq!(resource.status(), RecipeApplicabilityStatus::Applicable);
        assert_eq!(resource.variant(), Some(expected_variant));
        assert!(resource.rejection().is_none());
    }

    for (anchor, code) in [
        ("8300003", RecipeRejectionCode::UnsupportedSchema),
        ("8300004", RecipeRejectionCode::WrongFieldShape),
        ("8300005", RecipeRejectionCode::WrongClass),
        ("8300007", RecipeRejectionCode::WrongFieldShape),
        ("8300008", RecipeRejectionCode::WrongFieldShape),
        ("8300009", RecipeRejectionCode::WrongFieldShape),
        ("8300010", RecipeRejectionCode::WrongFieldShape),
        ("8300011", RecipeRejectionCode::WrongFieldShape),
    ] {
        let object = planner
            .inspect(&fixture.address(anchor), &mut AssetLoadBudget::default())
            .unwrap();
        let applicability = planner
            .capabilities_for(&object, &mut AssetLoadBudget::default())
            .unwrap();
        let resource = applicability
            .iter()
            .find(|entry| entry.recipe() == RecipeId::AudioClipStreamedResourceV1)
            .unwrap();
        let error = AudioClipResourceRecipe::lower(
            &planner,
            &object,
            PlanPayload::new(b"OggS".to_vec()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), Some(code));
        assert_eq!(resource.status(), RecipeApplicabilityStatus::Rejected);
        assert_eq!(resource.rejection(), Some(code));
        assert!(resource.variant().is_none());
    }

    let malformed = planner
        .inspect(&fixture.address("8300007"), &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        AudioClipResourceRecipe::lower(
            &planner,
            &malformed,
            PlanPayload::new(b"OggS".to_vec()),
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::InvalidMediaDescriptor {
            source: unity_asset_decode::media::MediaInspectionError::InvalidDescriptor {
                field: "m_Offset",
                reason: "field must be an unsigned integer",
            },
        })
    ));

    let object = planner
        .inspect(&fixture.address("8300000"), &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        AudioClipResourceRecipe::lower(
            &planner,
            &object,
            PlanPayload::new(Vec::new()),
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::InvalidPayload { .. })
    ));
}

#[test]
fn plan_builder_preserves_recipe_order_and_rejects_snapshot_overlap() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let transform = planner
        .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let first = changed(
        TransformRecipe::lower_transform(
            &planner,
            &transform,
            TransformChange::LocalPosition(Vector3::new(1.0, 2.0, 3.0)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    let duplicate = first.clone();
    let mut builder = MutationPlanBuilder::new(planner.workspace_id(), planner.revision());
    builder.append(first).unwrap();
    assert!(matches!(
        builder.append(duplicate),
        Err(MutationPlanBuilderError::OverlappingWrites { .. })
    ));
    let plan = builder.build().unwrap();
    assert_eq!(plan.operations()[0].ordinal(), 0);
    let canonical = String::from_utf8(plan.canonical_json().unwrap()).unwrap();
    assert!(!canonical.contains("recipe"));
    assert!(!canonical.contains("schema_provenance"));

    let wrong_revision = WorkspaceRevision::new(unity_asset::DigestV1::hash_bytes(b"other"));
    let mut wrong = MutationPlanBuilder::new(planner.workspace_id(), wrong_revision);
    let fragment = changed(
        TransformRecipe::lower_transform(
            &planner,
            &transform,
            TransformChange::LocalScale(Vector3::new(2.0, 2.0, 2.0)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    assert!(matches!(
        wrong.append(fragment),
        Err(MutationPlanBuilderError::RevisionMismatch { .. })
    ));
}

#[test]
fn snapshot_schema_and_value_guards_are_deterministic_across_repeated_inspection() {
    let fixture = Fixture::open("materials.prefab", MATERIAL_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let address = fixture.address("2100000");
    let first = planner
        .inspect(&address, &mut AssetLoadBudget::default())
        .unwrap();
    let second = planner
        .inspect(&address, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(first.provenance(), second.provenance());
    assert_eq!(first.provenance().class_id(), 21);
    assert!(first.provenance().schema_digest().is_some());

    let target = ReferenceTarget::object(fixture.address("2100001"));
    let first = changed(
        MaterialRecipe::lower(
            &planner,
            &first,
            "_MainTex",
            MaterialTextureChange::Retarget {
                expected: ReferenceTarget::null(),
                replacement: target.clone(),
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    let second = changed(
        MaterialRecipe::lower(
            &planner,
            &second,
            "_MainTex",
            MaterialTextureChange::Retarget {
                expected: ReferenceTarget::null(),
                replacement: target,
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    assert_eq!(first, second);
}

#[test]
fn recipe_lowering_rejects_an_object_inspected_from_a_stale_snapshot() {
    let mut fixture = Fixture::open("materials.prefab", MATERIAL_YAML);
    let first_snapshot = fixture.workspace.snapshot();
    let first_planner = SchemaRecipePlanner::new(&first_snapshot);
    let stale = first_planner
        .inspect(&fixture.address("2100000"), &mut AssetLoadBudget::default())
        .unwrap();

    let changed_yaml = MATERIAL_YAML.replacen("m_Scale: {x: 1, y: 1}", "m_Scale: {x: 2, y: 2}", 1);
    fs::write(fixture._directory.path().join(&fixture.alias), changed_yaml).unwrap();
    fixture.source = source_replacement::replace_source_path(
        &mut fixture.workspace,
        fixture.source,
        &fixture._directory.path().join(&fixture.alias),
        &fixture.alias,
    );
    let second_snapshot = fixture.workspace.snapshot();
    assert_ne!(first_snapshot.revision(), second_snapshot.revision());
    let second_planner = SchemaRecipePlanner::new(&second_snapshot);

    assert!(
        second_planner
            .capabilities_for(&stale, &mut AssetLoadBudget::default())
            .unwrap()
            .iter()
            .all(
                |capability| capability.status() == RecipeApplicabilityStatus::Rejected
                    && capability.rejection() == Some(RecipeRejectionCode::TargetInvalid)
            )
    );

    assert!(matches!(
        MaterialRecipe::lower(
            &second_planner,
            &stale,
            "_MainTex",
            MaterialTextureChange::Retarget {
                expected: ReferenceTarget::null(),
                replacement: ReferenceTarget::object(fixture.address("2100001")),
            },
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::InspectionContractMismatch)
    ));
}

#[test]
fn recipe_inspection_and_capability_discovery_have_exact_budget_boundaries() {
    let fixture = Fixture::open("materials.prefab", MATERIAL_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let address = fixture.address("2100000");

    let mut measured = AssetLoadBudget::default();
    planner.inspect(&address, &mut measured).unwrap();
    let inspect_usage = measured.usage();
    assert!(inspect_usage.entries > 1 && inspect_usage.bytes > 1);

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: inspect_usage.entries,
        max_bytes: inspect_usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let object = planner.inspect(&address, &mut exact).unwrap();
    assert_eq!(exact.usage(), inspect_usage);

    for limits in [
        AssetLoadLimits {
            max_entries: inspect_usage.entries - 1,
            max_bytes: inspect_usage.bytes,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_entries: inspect_usage.entries,
            max_bytes: inspect_usage.bytes - 1,
            ..AssetLoadLimits::default()
        },
    ] {
        let mut one_short = AssetLoadBudget::new(limits).unwrap();
        let result = planner.inspect(&address, &mut one_short);
        assert!(
            matches!(result, Err(RecipeError::Budget(_))),
            "expected a recipe budget error, got {result:?}"
        );
    }

    let mut measured = AssetLoadBudget::default();
    planner.capabilities_for(&object, &mut measured).unwrap();
    let capability_usage = measured.usage();
    assert!(capability_usage.entries > 6);
    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: capability_usage.entries,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    planner.capabilities_for(&object, &mut exact).unwrap();
    assert_eq!(exact.usage(), capability_usage);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: capability_usage.entries - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        planner.capabilities_for(&object, &mut one_short),
        Err(RecipeError::Budget(_))
    ));
}

fn long_persistent_call(fixture: &Fixture) -> PersistentCall {
    PersistentCall::new(
        ReferenceTarget::object(fixture.address("100000")),
        "A".repeat(4 * 1024),
        "M".repeat(4 * 1024),
        PersistentArgument::String("S".repeat(4 * 1024)),
        PersistentCallState::RuntimeOnly,
    )
    .unwrap()
}

#[test]
fn unity_event_output_allocations_obey_exact_and_one_short_budgets() {
    let fixture = Fixture::open("events.prefab", EVENT_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let object = planner
        .inspect(
            &fixture.address("11400001"),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let event_path = FieldPath::root().push_field("m_OnClick").unwrap();
    let run = |budget: &mut AssetLoadBudget| {
        UnityEventRecipe::lower(
            &planner,
            &object,
            event_path.clone(),
            UnityEventEdit::Add {
                call: long_persistent_call(&fixture),
                shape: PersistentCallShape::WithTargetAssemblyTypeName,
            },
            budget,
        )
        .map(|_| ())
        .map_err(|error| matches!(error, RecipeError::Budget(_)))
    };

    let mut measured = AssetLoadBudget::default();
    run(&mut measured).unwrap();
    let usage = measured.usage();
    assert!(usage.entries > 1 && usage.bytes > 12 * 1024);

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    run(&mut exact).unwrap();
    assert_eq!(exact.usage(), usage);

    for limits in [
        AssetLoadLimits {
            max_entries: usage.entries - 1,
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        },
    ] {
        let mut one_short = AssetLoadBudget::new(limits).unwrap();
        assert_eq!(run(&mut one_short), Err(true));
    }
}

fn many_material_entries(count: usize) -> String {
    let mut yaml = String::from(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!21 &2100000\nMaterial:\n  m_SavedProperties:\n    m_TexEnvs:\n",
    );
    for index in 0..count {
        yaml.push_str(&format!(
            "    - _Other{index}:\n        m_Texture: {{fileID: 0}}\n        m_Scale: {{x: 1, y: 1}}\n        m_Offset: {{x: 0, y: 0}}\n"
        ));
    }
    yaml.push_str(
        "    - _MainTex:\n        m_Texture: {fileID: 0}\n        m_Scale: {x: 1, y: 1}\n        m_Offset: {x: 0, y: 0}\n",
    );
    yaml
}

#[test]
fn material_scan_to_the_last_property_is_budgeted() {
    let fixture = Fixture::open("many-materials.prefab", &many_material_entries(64));
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let object = planner
        .inspect(&fixture.address("2100000"), &mut AssetLoadBudget::default())
        .unwrap();
    let run = |budget: &mut AssetLoadBudget| {
        MaterialRecipe::lower(
            &planner,
            &object,
            "_MainTex",
            MaterialTextureChange::Retarget {
                expected: ReferenceTarget::null(),
                replacement: ReferenceTarget::object(fixture.address("2100000")),
            },
            budget,
        )
        .map(|_| ())
        .map_err(|error| matches!(error, RecipeError::Budget(_)))
    };
    let mut measured = AssetLoadBudget::default();
    run(&mut measured).unwrap();
    let usage = measured.usage();
    assert!(usage.entries > 64 && usage.bytes > 1);

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    run(&mut exact).unwrap();
    assert_eq!(exact.usage(), usage);

    for limits in [
        AssetLoadLimits {
            max_entries: usage.entries - 1,
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        },
    ] {
        let mut one_short = AssetLoadBudget::new(limits).unwrap();
        assert_eq!(run(&mut one_short), Err(true));
    }
}

fn equivalent_plan_pair(fragment: MutationPlanFragment) -> (MutationPlan, MutationPlan) {
    let direct = MutationPlan::new(
        fragment.workspace_id(),
        fragment.base_revision(),
        fragment.sources().to_vec(),
        fragment.payloads().to_vec(),
        fragment.actions().to_vec(),
    )
    .unwrap();
    let mut builder = MutationPlanBuilder::new(fragment.workspace_id(), fragment.base_revision());
    builder.append(fragment).unwrap();
    let recipe = builder.build().unwrap();
    assert_eq!(
        recipe.canonical_json().unwrap(),
        direct.canonical_json().unwrap()
    );
    (recipe, direct)
}

fn assert_prepare_equivalent(fixture: &Fixture, fragment: MutationPlanFragment) {
    let (recipe, direct) = equivalent_plan_pair(fragment);
    let recipe = fixture
        .workspace
        .prepare(
            recipe,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let direct = fixture
        .workspace
        .prepare(
            direct,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(recipe.report(), direct.report());
    assert_eq!(recipe.artifact_usage(), direct.artifact_usage());
    assert_eq!(
        recipe.view().reference_graph().facts(),
        direct.view().reference_graph().facts()
    );
}

#[test]
fn every_retained_recipe_matches_its_direct_primitive_prepare_result() {
    let mut covered = Vec::new();

    {
        let fixture = Fixture::open("reference-equivalence.prefab", MATERIAL_YAML);
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let object = planner
            .inspect(&fixture.address("2100000"), &mut AssetLoadBudget::default())
            .unwrap();
        let path = FieldPath::root()
            .push_field("m_SavedProperties")
            .unwrap()
            .push_field("m_TexEnvs")
            .unwrap()
            .push_index(0)
            .unwrap()
            .push_field("second")
            .unwrap()
            .push_field("m_Texture")
            .unwrap();
        let fragment = changed(
            planner
                .lower_reference(
                    &object,
                    path,
                    ReferenceTarget::null(),
                    ReferenceTarget::object(fixture.address("2100001")),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap(),
        );
        assert_prepare_equivalent(&fixture, fragment);
        covered.push(RecipeId::ReferenceRetargetV1);
    }

    {
        let fixture = Fixture::open("transform-equivalence.prefab", HIERARCHY_YAML);
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let object = planner
            .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
            .unwrap();
        let fragment = changed(
            TransformRecipe::lower_transform(
                &planner,
                &object,
                TransformChange::LocalPosition(Vector3::new(3.0, 2.0, 1.0)),
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        );
        assert_prepare_equivalent(&fixture, fragment);
        covered.push(RecipeId::TransformV1);
    }

    {
        let fixture = Fixture::open("material-equivalence.prefab", MATERIAL_YAML);
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let object = planner
            .inspect(&fixture.address("2100000"), &mut AssetLoadBudget::default())
            .unwrap();
        let fragment = changed(
            MaterialRecipe::lower(
                &planner,
                &object,
                "_MainTex",
                MaterialTextureChange::SetScaleOffset {
                    scale: Vector2::new(2.0, 3.0),
                    offset: Vector2::new(0.25, 0.5),
                },
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        );
        assert_prepare_equivalent(&fixture, fragment);
        covered.push(RecipeId::MaterialTextureEnvironmentV1);
    }

    {
        let fixture = Fixture::open("event-equivalence.prefab", EVENT_YAML);
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let object = planner
            .inspect(
                &fixture.address("11400001"),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let call = PersistentCall::new(
            ReferenceTarget::object(fixture.address("100000")),
            "Example.Target, Example",
            "Prepared",
            PersistentArgument::Void,
            PersistentCallState::RuntimeOnly,
        )
        .unwrap();
        let fragment = changed(
            UnityEventRecipe::lower(
                &planner,
                &object,
                FieldPath::root().push_field("m_OnClick").unwrap(),
                UnityEventEdit::Add {
                    call,
                    shape: PersistentCallShape::WithTargetAssemblyTypeName,
                },
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        );
        assert_prepare_equivalent(&fixture, fragment);
        covered.push(RecipeId::UnityEventPersistentCallsV1);
    }

    {
        let fixture = Fixture::open("hierarchy-equivalence.prefab", HIERARCHY_YAML);
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let intent = hierarchy_intent(
            &planner,
            fixture.address("2"),
            HierarchyDestinationV1::parent(fixture.address("3"), HierarchyPlacementV1::Last),
        );
        let fragment = changed(
            HierarchyRecipe::lower(&planner, &intent, &mut AssetLoadBudget::default()).unwrap(),
        );
        assert_prepare_equivalent(&fixture, fragment);
        covered.push(RecipeId::HierarchyReparentV1);
    }

    {
        let fixture = Fixture::open("resource-equivalence.prefab", RESOURCE_YAML);
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let object = planner
            .inspect(&fixture.address("8300001"), &mut AssetLoadBudget::default())
            .unwrap();
        let fragment = changed(
            AudioClipResourceRecipe::lower(
                &planner,
                &object,
                PlanPayload::new(b"OggS-equivalence".to_vec()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        );
        assert_prepare_equivalent(&fixture, fragment);
        covered.push(RecipeId::AudioClipStreamedResourceV1);
    }

    assert_eq!(
        covered,
        recipe_capabilities()
            .recipes()
            .iter()
            .map(|recipe| recipe.id())
            .collect::<Vec<_>>()
    );
}

#[test]
fn recipe_and_direct_plan_report_the_same_external_source_conflict() {
    let fixture = Fixture::open("recipe-conflict.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let object = planner
        .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let fragment = changed(
        TransformRecipe::lower_transform(
            &planner,
            &object,
            TransformChange::LocalScale(Vector3::new(2.0, 2.0, 2.0)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    let (recipe, direct) = equivalent_plan_pair(fragment);
    let changed_source = HIERARCHY_YAML.replacen(
        "m_LocalScale: {x: 1, y: 1, z: 1}",
        "m_LocalScale: {x: 9, y: 9, z: 9}",
        1,
    );
    fs::write(
        fixture._directory.path().join(&fixture.alias),
        changed_source,
    )
    .unwrap();

    let recipe = fixture
        .workspace
        .prepare(
            recipe,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    let direct = fixture
        .workspace
        .prepare(
            direct,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert_eq!(recipe.report(), direct.report());
    let diagnostic = &recipe.report().diagnostics()[0];
    assert!(diagnostic.expected_fingerprint().is_some());
    assert!(diagnostic.actual_fingerprint().is_some());
}
