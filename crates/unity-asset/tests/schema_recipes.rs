use std::fs;
use std::path::PathBuf;

use unity_asset::environment::{BinarySource, BinarySourceKind, Environment};
use unity_asset::schema::{
    AudioClipResourceRecipe, ChildPlacement, DeclaredUnityVersion, HierarchyNode, HierarchyRecipe,
    HierarchyState, MaterialRecipe, MaterialTextureChange, PersistentArgument, PersistentCall,
    PersistentCallShape, PersistentCallState, RecipeApplicabilityStatus, RecipeError, RecipeId,
    RecipeLowering, RecipeRejectionCode, RectTransformChange, SchemaOrigin, SchemaRecipePlanner,
    SchemaVariantId, TransformChange, TransformRecipe, UnityEventEdit, UnityEventRecipe, Vector2,
    Vector3, recipe_capabilities,
};
use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationPlanBuilder,
    MutationPlanBuilderError, MutationPlanFragment, MutationValue, MutationValueRef, PlanPayload,
    PrepareOptions, PublicationTarget, ReferenceTarget, SequenceMutation,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, FieldPath, ObjectAddress, SourceLocator, UnityValue,
    WorkspaceRevision,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};

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
"#;

struct Fixture {
    _directory: tempfile::TempDir,
    workspace: AssetWorkspace,
    alias: String,
}

impl Fixture {
    fn open(alias: &str, yaml: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(alias);
        fs::write(&path, yaml).unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_path(&path, &mut AssetLoadBudget::default())
            .unwrap();
        Self {
            _directory: directory,
            workspace,
            alias: alias.to_owned(),
        }
    }

    fn address(&self, anchor: &str) -> ObjectAddress {
        ObjectAddress::yaml(SourceLocator::path(&self.alias).unwrap(), anchor).unwrap()
    }
}

fn changed(lowering: RecipeLowering) -> unity_asset::workspace::MutationPlanFragment {
    lowering
        .into_fragment()
        .expect("recipe should produce a changed fragment")
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

        let nodes = if anchor == "1" {
            let child_address = fixture.address("2");
            let child = planner
                .inspect(&child_address, &mut AssetLoadBudget::default())
                .unwrap();
            vec![
                HierarchyNode::new(object, None, vec![child_address]),
                HierarchyNode::new(child, Some(fixture.address(anchor)), Vec::new()),
            ]
        } else {
            vec![HierarchyNode::new(object, None, Vec::new())]
        };
        let state = HierarchyState::new(nodes, &mut AssetLoadBudget::default()).unwrap();
        let lowering = HierarchyRecipe::reparent(
            &planner,
            &state,
            &fixture.address(anchor),
            None,
            ChildPlacement::Append,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(lowering.report().variant(), hierarchy.variant().unwrap());
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

fn hierarchy_nodes(fixture: &Fixture, planner: &SchemaRecipePlanner<'_>) -> Vec<HierarchyNode> {
    let one = planner
        .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let two = planner
        .inspect(&fixture.address("2"), &mut AssetLoadBudget::default())
        .unwrap();
    let three = planner
        .inspect(&fixture.address("3"), &mut AssetLoadBudget::default())
        .unwrap();
    vec![
        HierarchyNode::new(one, None, vec![fixture.address("2")]),
        HierarchyNode::new(two, Some(fixture.address("1")), Vec::new()),
        HierarchyNode::new(three, None, Vec::new()),
    ]
}

fn hierarchy_state(fixture: &Fixture, planner: &SchemaRecipePlanner<'_>) -> HierarchyState {
    HierarchyState::new(
        hierarchy_nodes(fixture, planner),
        &mut AssetLoadBudget::default(),
    )
    .unwrap()
}

#[test]
fn hierarchy_validation_and_output_have_exact_budget_boundaries() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);

    let mut measured = AssetLoadBudget::default();
    HierarchyState::new(hierarchy_nodes(&fixture, &planner), &mut measured).unwrap();
    let state_usage = measured.usage();
    assert!(state_usage.entries > 1 && state_usage.bytes > 1);
    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: state_usage.entries,
        max_bytes: state_usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    HierarchyState::new(hierarchy_nodes(&fixture, &planner), &mut exact).unwrap();
    assert_eq!(exact.usage(), state_usage);
    for limits in [
        AssetLoadLimits {
            max_entries: state_usage.entries - 1,
            max_bytes: state_usage.bytes,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_entries: state_usage.entries,
            max_bytes: state_usage.bytes - 1,
            ..AssetLoadLimits::default()
        },
    ] {
        let mut one_short = AssetLoadBudget::new(limits).unwrap();
        assert!(matches!(
            HierarchyState::new(hierarchy_nodes(&fixture, &planner), &mut one_short),
            Err(RecipeError::Budget(_))
        ));
    }

    let state = hierarchy_state(&fixture, &planner);
    let run = |budget: &mut AssetLoadBudget| {
        HierarchyRecipe::reparent(
            &planner,
            &state,
            &fixture.address("2"),
            Some(&fixture.address("3")),
            ChildPlacement::Append,
            budget,
        )
        .map(|_| ())
        .map_err(|error| matches!(error, RecipeError::Budget(_)))
    };
    let mut measured = AssetLoadBudget::default();
    run(&mut measured).unwrap();
    let output_usage = measured.usage();
    assert!(output_usage.entries > 1 && output_usage.bytes > 1);
    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: output_usage.entries,
        max_bytes: output_usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    run(&mut exact).unwrap();
    assert_eq!(exact.usage(), output_usage);
    for limits in [
        AssetLoadLimits {
            max_entries: output_usage.entries - 1,
            max_bytes: output_usage.bytes,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_entries: output_usage.entries,
            max_bytes: output_usage.bytes - 1,
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
    let state = hierarchy_state(&fixture, &planner);
    let fragment = changed(
        HierarchyRecipe::reparent(
            &planner,
            &state,
            &fixture.address("2"),
            Some(&fixture.address("3")),
            ChildPlacement::Append,
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
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
fn hierarchy_move_indices_name_final_sequence_positions() {
    let fixture = Fixture::open("ordered-hierarchy.prefab", ORDERED_HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let mut nodes = Vec::new();
    for anchor in ["1", "2", "3", "4"] {
        let object = planner
            .inspect(&fixture.address(anchor), &mut AssetLoadBudget::default())
            .unwrap();
        let parent = (anchor != "1").then(|| fixture.address("1"));
        let children = if anchor == "1" {
            ["2", "3", "4"]
                .into_iter()
                .map(|child| fixture.address(child))
                .collect()
        } else {
            Vec::new()
        };
        nodes.push(HierarchyNode::new(object, parent, children));
    }
    let state = HierarchyState::new(nodes, &mut AssetLoadBudget::default()).unwrap();

    for (child, placement, expected_from, expected_to) in [
        ("2", ChildPlacement::At(2), 0, 2),
        ("4", ChildPlacement::At(0), 2, 0),
    ] {
        let fragment = changed(
            HierarchyRecipe::reparent(
                &planner,
                &state,
                &fixture.address(child),
                Some(&fixture.address("1")),
                placement,
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
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
fn binary_hierarchy_recipe_uses_the_same_guarded_sequence_contract() {
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
        assert_eq!(object.class().class_id, 4);
        assert_eq!(object.class().class_name, "Transform");
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

    let state = HierarchyState::new(
        vec![
            HierarchyNode::new(parent, None, vec![child_address.clone()]),
            HierarchyNode::new(child, Some(parent_address), Vec::new()),
        ],
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let fragment = changed(
        HierarchyRecipe::reparent(
            &planner,
            &state,
            &child_address,
            None,
            ChildPlacement::Append,
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
    );
    assert_eq!(fragment.actions().len(), 2);
    assert!(matches!(
        &fragment.actions()[0],
        GenericMutation::ReferenceReplace { .. }
    ));
    assert!(matches!(
        &fragment.actions()[1],
        GenericMutation::SequenceEdit {
            edit: SequenceMutation::Remove { .. },
            ..
        }
    ));
}

#[test]
fn cross_source_reference_commit_is_discoverable_by_read_only_environment() {
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin",
    );
    let directory = tempfile::tempdir().unwrap();
    let owner_path = directory.path().join("owner.assets");
    let dependency_directory = directory.path().join("deps");
    let target_path = dependency_directory.join("target.assets");
    fs::create_dir_all(&dependency_directory).unwrap();
    fs::copy(&sample, &owner_path).unwrap();
    fs::copy(&sample, &target_path).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&owner_path, &mut AssetLoadBudget::default())
        .unwrap();
    workspace
        .load_path(&target_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let owner_locator = SourceLocator::path("owner.assets").unwrap();
    let target_locator = SourceLocator::path("target.assets").unwrap();
    let owner_parent = ObjectAddress::binary_direct(owner_locator.clone(), 1).unwrap();
    let owner_child = ObjectAddress::binary_direct(owner_locator, 2).unwrap();
    let target_parent = ObjectAddress::binary_direct(target_locator, 1).unwrap();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let child = planner
        .inspect(&owner_child, &mut AssetLoadBudget::default())
        .unwrap();
    let lowering = planner
        .lower_reference(
            &child,
            FieldPath::root().push_field("m_Father").unwrap(),
            ReferenceTarget::object(owner_parent),
            ReferenceTarget::object(target_parent),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let fragment = changed(lowering);
    let mut builder = MutationPlanBuilder::new(snapshot.revision());
    builder.append(fragment).unwrap();
    let prepared = workspace
        .prepare(
            builder.build().unwrap(),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    workspace
        .commit(
            prepared,
            PublicationTarget::in_place(directory.path()).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let owner_path = fs::canonicalize(owner_path).unwrap();
    let target_path = fs::canonicalize(target_path).unwrap();
    let mut environment = Environment::new();
    environment
        .load_file(&owner_path, &mut AssetLoadBudget::default())
        .unwrap();
    let owner_source = BinarySource::path(&owner_path);
    let child_key = environment
        .binary_object_infos()
        .find(|object| object.source == &owner_source && object.object.path_id() == 2)
        .expect("owner fixture must retain its child Transform")
        .key();
    assert_eq!(
        environment
            .resolve_pptr_path_key(&child_key, "m_Father", &mut AssetLoadBudget::default())
            .unwrap(),
        None
    );

    let resolved = environment
        .resolve_pptr_path_key_best_effort(&child_key, "m_Father", &mut AssetLoadBudget::default())
        .unwrap()
        .expect("best-effort lookup must load the nested dependency");
    assert_eq!(resolved.source, BinarySource::path(&target_path));
    assert_eq!(resolved.source_kind, BinarySourceKind::SerializedFile);
    assert_eq!(resolved.asset_index, None);
    assert_eq!(resolved.path_id, 1);
    assert!(
        environment
            .binary_assets()
            .contains_key(&BinarySource::path(target_path))
    );
}

#[test]
fn hierarchy_recipe_rejects_self_cycles_missing_and_inconsistent_membership() {
    let fixture = Fixture::open("hierarchy.prefab", HIERARCHY_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let state = hierarchy_state(&fixture, &planner);

    assert!(matches!(
        HierarchyRecipe::reparent(
            &planner,
            &state,
            &fixture.address("2"),
            Some(&fixture.address("2")),
            ChildPlacement::Append,
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::SelfParent { .. })
    ));
    assert!(matches!(
        HierarchyRecipe::reparent(
            &planner,
            &state,
            &fixture.address("1"),
            Some(&fixture.address("2")),
            ChildPlacement::Append,
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::HierarchyCycle { .. })
    ));
    assert!(matches!(
        HierarchyRecipe::reparent(
            &planner,
            &state,
            &fixture.address("2"),
            Some(&fixture.address("99")),
            ChildPlacement::Append,
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::MissingParent { .. })
    ));

    let one = planner
        .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let two = planner
        .inspect(&fixture.address("2"), &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        HierarchyState::new(
            vec![
                HierarchyNode::new(one, None, Vec::new()),
                HierarchyNode::new(two, Some(fixture.address("1")), Vec::new()),
            ],
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::ParentChildMismatch { .. })
    ));

    let one = planner
        .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let two = planner
        .inspect(&fixture.address("2"), &mut AssetLoadBudget::default())
        .unwrap();
    let three = planner
        .inspect(&fixture.address("3"), &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        HierarchyState::new(
            vec![
                HierarchyNode::new(one, None, vec![fixture.address("3")]),
                HierarchyNode::new(two, Some(fixture.address("1")), Vec::new()),
                HierarchyNode::new(three, None, Vec::new()),
            ],
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::ParentChildMismatch { .. })
    ));

    let dangling = planner
        .inspect(&fixture.address("6"), &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        HierarchyState::new(
            vec![HierarchyNode::new(
                dangling,
                None,
                vec![fixture.address("99")],
            )],
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::MissingChild { .. })
    ));

    let external = planner
        .inspect(&fixture.address("7"), &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        HierarchyState::new(
            vec![HierarchyNode::new(
                external,
                None,
                vec![fixture.address("8")],
            )],
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::UnresolvedReference { .. })
    ));

    assert!(matches!(
        HierarchyRecipe::reparent(
            &planner,
            &state,
            &fixture.address("2"),
            Some(&fixture.address("1")),
            ChildPlacement::At(2),
            &mut AssetLoadBudget::default(),
        ),
        Err(RecipeError::ChildPlacementOutOfBounds {
            index: 2,
            maximum: 0,
        })
    ));
}

#[test]
fn audio_clip_recipe_classifies_all_candidate_combinations_without_allocating_cab_state() {
    let fixture = Fixture::open("audio.prefab", RESOURCE_YAML);
    let snapshot = fixture.workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);

    for (anchor, expected_path) in [
        ("8300000", "$.m_Resource"),
        ("8300001", "$.m_StreamData"),
        ("8300002", "$.m_Resource"),
    ] {
        let object = planner
            .inspect(&fixture.address(anchor), &mut AssetLoadBudget::default())
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
    }

    for (anchor, code) in [
        ("8300003", RecipeRejectionCode::UnsupportedSchema),
        ("8300004", RecipeRejectionCode::WrongFieldShape),
        ("8300005", RecipeRejectionCode::WrongClass),
        ("8300006", RecipeRejectionCode::WrongFieldShape),
        ("8300007", RecipeRejectionCode::WrongFieldShape),
        ("8300008", RecipeRejectionCode::WrongFieldShape),
    ] {
        let object = planner
            .inspect(&fixture.address(anchor), &mut AssetLoadBudget::default())
            .unwrap();
        let error = AudioClipResourceRecipe::lower(
            &planner,
            &object,
            PlanPayload::new(b"OggS".to_vec()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), Some(code));
    }

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
    let mut builder = MutationPlanBuilder::new(planner.revision());
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
    let mut wrong = MutationPlanBuilder::new(wrong_revision);
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
    fixture
        .workspace
        .load_path(
            fixture._directory.path().join(&fixture.alias),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
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
        fragment.base_revision(),
        fragment.sources().to_vec(),
        fragment.payloads().to_vec(),
        fragment.actions().to_vec(),
    )
    .unwrap();
    let mut builder = MutationPlanBuilder::new(fragment.base_revision());
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
        let state = hierarchy_state(&fixture, &planner);
        let fragment = changed(
            HierarchyRecipe::reparent(
                &planner,
                &state,
                &fixture.address("2"),
                Some(&fixture.address("3")),
                ChildPlacement::Append,
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
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
