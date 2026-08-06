use std::io::{self, Read};
use std::mem::size_of;

use unity_asset::workspace::{
    FieldGuard, GenericMutation, MutationField, MutationOperation, MutationPlan, MutationPlanError,
    MutationPlanReadError, MutationValue, MutationValueRef, ObjectGuard, PlanBytes, PlanPayload,
    ReferenceTarget, SequenceMutation, SourceExpectation, UnsafeRawAcknowledgement,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, ContainmentKind, DigestV1, FieldPath,
    ObjectAddress, SourceFingerprint, SourceKind, SourceLocator, SourceMemberId, WorkspaceId,
    WorkspaceRevision,
};

fn digest(label: &[u8]) -> DigestV1 {
    DigestV1::hash_bytes(label)
}

fn workspace_id() -> WorkspaceId {
    WorkspaceId::from_u128(1).unwrap()
}

fn read_json_plan(bytes: &[u8]) -> Result<MutationPlan, MutationPlanReadError> {
    MutationPlan::from_json_slice(bytes, &mut AssetLoadBudget::default())
}

fn read_json_value(value: serde_json::Value) -> Result<MutationPlan, MutationPlanReadError> {
    read_json_plan(&serde_json::to_vec(&value).unwrap())
}

#[allow(dead_code)]
struct MutationPlanWireLayout {
    version: u8,
    workspace_id: Option<WorkspaceId>,
    base_revision: WorkspaceRevision,
    sources: Vec<SourceExpectation>,
    payloads: Vec<PlanPayload>,
    operations: Vec<MutationOperation>,
}

fn json_structure(value: &serde_json::Value) -> (u64, u64) {
    fn visit(value: &serde_json::Value, entries: &mut u64, string_bytes: &mut u64) {
        if let serde_json::Value::String(value) = value {
            *string_bytes += u64::try_from(value.len()).unwrap();
        }
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    *entries += 1;
                    visit(value, entries, string_bytes);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values() {
                    *entries += 1;
                    visit(value, entries, string_bytes);
                }
            }
            _ => {}
        }
    }

    let mut entries = 1;
    let mut string_bytes = 0;
    visit(value, &mut entries, &mut string_bytes);
    (entries, string_bytes)
}

fn expected_json_budget_bytes(encoded: &[u8]) -> u64 {
    const PARSER_FIXED_WORK_BYTES: u64 = 256 * 1024;
    const PARSER_WORK_BYTES_PER_INPUT_BYTE: u64 = 6;
    const WIRE_LAYOUT_BYTES_PER_ENTRY: u64 = 512;
    const FROM_WIRE_TRANSITION_BYTES_PER_ENTRY: u64 = 1024;
    const MATERIALIZATION_FIXED_BYTES: u64 = 64 * 1024;

    let encoded_bytes = u64::try_from(encoded.len()).unwrap();
    let value: serde_json::Value = serde_json::from_slice(encoded).unwrap();
    let (entries, string_bytes) = json_structure(&value);
    let root_layout = u64::try_from(size_of::<MutationPlanWireLayout>()).unwrap();
    let decoded_plan_bytes = string_bytes.div_ceil(2);

    PARSER_FIXED_WORK_BYTES
        + encoded_bytes * (PARSER_WORK_BYTES_PER_INPUT_BYTE + 1)
        + MATERIALIZATION_FIXED_BYTES
        + root_layout
        + entries * WIRE_LAYOUT_BYTES_PER_ENTRY
        + entries * FROM_WIRE_TRANSITION_BYTES_PER_ENTRY
        + encoded_bytes * 2
        + decoded_plan_bytes
}

fn binary_locator() -> SourceLocator {
    SourceLocator::path("packages/game.zip")
        .unwrap()
        .child(
            ContainmentKind::Archive,
            SourceMemberId::new("content/game.bundle").unwrap(),
        )
        .unwrap()
        .child(
            ContainmentKind::Bundle,
            SourceMemberId::new("CAB-main.assets").unwrap(),
        )
        .unwrap()
}

fn yaml_locator() -> SourceLocator {
    SourceLocator::path("Assets/Scenes/Main.unity").unwrap()
}

fn binary_address() -> ObjectAddress {
    ObjectAddress::binary_at(binary_locator(), -7).unwrap()
}

fn raw_binary_address() -> ObjectAddress {
    ObjectAddress::binary_at(binary_locator(), -8).unwrap()
}

fn yaml_address() -> ObjectAddress {
    ObjectAddress::yaml(yaml_locator(), "100100000".parse().unwrap()).unwrap()
}

fn expectations() -> Vec<SourceExpectation> {
    vec![
        SourceExpectation::new(
            binary_locator(),
            SourceFingerprint::from_bytes(SourceKind::SerializedFile, b"binary source"),
        ),
        SourceExpectation::new(
            yaml_locator(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, b"yaml source"),
        ),
    ]
}

fn sequence_action(edit: SequenceMutation) -> GenericMutation {
    GenericMutation::SequenceEdit {
        target: binary_address(),
        path: FieldPath::root().push_field("m_Items").unwrap(),
        guard: FieldGuard::new(digest(b"sequence schema"), digest(b"sequence values")),
        edit,
    }
}

fn sequence_plan(edit: SequenceMutation) -> MutationPlan {
    MutationPlan::new(
        workspace_id(),
        WorkspaceRevision::new(digest(b"sequence revision")),
        vec![expectations().remove(0)],
        Vec::new(),
        vec![sequence_action(edit)],
    )
    .unwrap()
}

fn sample_plan() -> MutationPlan {
    let resource = PlanPayload::new(PlanBytes::new(vec![0, 1, 2, 0xfe, 0xff]));
    let raw = PlanPayload::new(PlanBytes::new(vec![0x13, 0x37, 0, 0xaa]));
    let resource_digest = resource.digest();
    let raw_digest = raw.digest();
    let replacement = MutationValue::object(vec![
        MutationField::new("z_last", MutationValue::unsigned(u64::MAX)).unwrap(),
        MutationField::new("a_first", MutationValue::float64(-0.0)).unwrap(),
        MutationField::new("bytes", MutationValue::bytes(PlanBytes::new(vec![0, 0xff]))).unwrap(),
    ])
    .unwrap();

    MutationPlan::new(
        workspace_id(),
        WorkspaceRevision::new(digest(b"workspace revision")),
        expectations(),
        vec![raw, resource],
        vec![
            GenericMutation::FieldReplace {
                target: binary_address(),
                path: FieldPath::root().push_field("m_Name").unwrap(),
                guard: FieldGuard::new(digest(b"name schema"), digest(b"old name")),
                replacement: MutationValue::string("Player").unwrap(),
            },
            GenericMutation::ReferenceReplace {
                target: binary_address(),
                path: FieldPath::root()
                    .push_field("m_Texture")
                    .unwrap()
                    .push_field("m_Ptr")
                    .unwrap(),
                schema_digest: digest(b"reference schema"),
                expected: ReferenceTarget::null(),
                replacement: ReferenceTarget::object(yaml_address()),
            },
            GenericMutation::SchemaReplace {
                target: yaml_address(),
                guard: ObjectGuard::new(digest(b"yaml schema"), digest(b"old yaml object")),
                replacement,
            },
            GenericMutation::ResourceReplace {
                target: binary_address(),
                path: FieldPath::root().push_field("m_StreamData").unwrap(),
                guard: FieldGuard::new(digest(b"stream schema"), digest(b"old stream")),
                payload: resource_digest,
            },
            GenericMutation::SequenceEdit {
                target: binary_address(),
                path: FieldPath::root().push_field("m_InsertItems").unwrap(),
                guard: FieldGuard::new(digest(b"insert schema"), digest(b"insert values")),
                edit: SequenceMutation::Insert {
                    index: 2,
                    value: MutationValue::string("third").unwrap(),
                },
            },
            GenericMutation::SequenceEdit {
                target: binary_address(),
                path: FieldPath::root().push_field("m_ReplaceItems").unwrap(),
                guard: FieldGuard::new(digest(b"replace schema"), digest(b"replace values")),
                edit: SequenceMutation::Replace {
                    index: 0,
                    value: MutationValue::reference(ReferenceTarget::object(yaml_address())),
                },
            },
            GenericMutation::SequenceEdit {
                target: binary_address(),
                path: FieldPath::root().push_field("m_RemoveItems").unwrap(),
                guard: FieldGuard::new(digest(b"remove schema"), digest(b"remove values")),
                edit: SequenceMutation::Remove { index: 4 },
            },
            GenericMutation::SequenceEdit {
                target: binary_address(),
                path: FieldPath::root().push_field("m_MoveItems").unwrap(),
                guard: FieldGuard::new(digest(b"move schema"), digest(b"move values")),
                edit: SequenceMutation::Move { from: 3, to: 1 },
            },
            GenericMutation::SequenceEdit {
                target: binary_address(),
                path: FieldPath::root().push_field("m_ClearItems").unwrap(),
                guard: FieldGuard::new(digest(b"clear schema"), digest(b"clear values")),
                edit: SequenceMutation::Clear,
            },
            GenericMutation::UnsafeRawReplace {
                target: raw_binary_address(),
                expected_raw_digest: digest(b"old raw object"),
                payload: raw_digest,
                acknowledgement: UnsafeRawAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
            },
        ],
    )
    .unwrap()
}

#[test]
fn all_mutation_primitives_have_stable_canonical_json() {
    let plan = sample_plan();
    let bytes = plan.canonical_json().unwrap();
    let golden = include_bytes!("mutation_plan_v3_all_operations.json");

    if bytes != golden {
        let mismatch = bytes
            .iter()
            .zip(golden)
            .position(|(actual, expected)| actual != expected)
            .unwrap_or_else(|| bytes.len().min(golden.len()));
        let start = mismatch.saturating_sub(80);
        let actual_end = bytes.len().min(mismatch.saturating_add(80));
        let expected_end = golden.len().min(mismatch.saturating_add(80));
        panic!(
            "canonical JSON first differs at byte {mismatch}; actual len {}, expected len {}; \
             actual context: {}; expected context: {}",
            bytes.len(),
            golden.len(),
            String::from_utf8_lossy(&bytes[start..actual_end]),
            String::from_utf8_lossy(&golden[start..expected_end]),
        );
    }
    assert_eq!(plan.version(), 3);
    assert_eq!(plan.workspace_id(), workspace_id());
    assert_eq!(serde_json::to_vec(&plan).unwrap(), bytes);
    assert_eq!(read_json_plan(&bytes).unwrap(), plan);
    assert_eq!(
        plan.operations()
            .iter()
            .map(|operation| operation.ordinal())
            .collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
    );
}

#[test]
fn canonical_identity_is_bound_to_the_workspace() {
    let first = sample_plan();
    let second = MutationPlan::new(
        WorkspaceId::from_u128(2).unwrap(),
        first.base_revision(),
        first.sources().to_vec(),
        first.payloads().to_vec(),
        first
            .operations()
            .iter()
            .map(|operation| operation.action().clone())
            .collect(),
    )
    .unwrap();

    assert_ne!(
        first.canonical_json().unwrap(),
        second.canonical_json().unwrap()
    );
    assert_ne!(first.digest().unwrap(), second.digest().unwrap());
}

#[test]
fn every_sequence_edit_round_trips_through_json_and_yaml() {
    let variants = [
        (
            "insert",
            SequenceMutation::Insert {
                index: 2,
                value: MutationValue::string("inserted").unwrap(),
            },
        ),
        (
            "replace",
            SequenceMutation::Replace {
                index: 1,
                value: MutationValue::reference(ReferenceTarget::object(yaml_address())),
            },
        ),
        ("remove", SequenceMutation::Remove { index: 4 }),
        ("move", SequenceMutation::Move { from: 3, to: 1 }),
        ("clear", SequenceMutation::Clear),
    ];

    for (name, edit) in variants {
        let plan = sequence_plan(edit);
        let canonical = plan.canonical_json().unwrap();
        assert_eq!(serde_json::to_vec(&plan).unwrap(), canonical, "{name}");
        assert_eq!(read_json_plan(&canonical).unwrap(), plan, "{name}");

        let mut yaml_budget = AssetLoadBudget::default();
        let from_yaml = MutationPlan::from_yaml_slice(&canonical, &mut yaml_budget).unwrap();
        assert_eq!(from_yaml, plan, "{name}");
        assert_eq!(from_yaml.canonical_json().unwrap(), canonical, "{name}");
    }
}

#[test]
fn sequence_move_rejects_a_noop_with_a_structured_error() {
    let error = MutationPlan::new(
        workspace_id(),
        WorkspaceRevision::new(digest(b"sequence revision")),
        vec![expectations().remove(0)],
        Vec::new(),
        vec![sequence_action(SequenceMutation::Move { from: 7, to: 7 })],
    )
    .unwrap_err();
    assert!(matches!(
        error,
        MutationPlanError::NoopSequenceMove { index: 7 }
    ));

    let mut value =
        serde_json::to_value(sequence_plan(SequenceMutation::Move { from: 7, to: 8 })).unwrap();
    value["operations"][0]["action"]["edit"]["to"] = serde_json::json!(7);
    assert!(matches!(
        read_json_value(value),
        Err(MutationPlanReadError::Contract(
            MutationPlanError::NoopSequenceMove { index: 7 }
        ))
    ));
}

#[test]
fn maximum_depth_reference_value_round_trips_through_json_and_yaml() {
    let deepest_reference = (0..58).fold(
        MutationValue::reference(ReferenceTarget::object(binary_address())),
        |value, level| {
            MutationValue::object(vec![
                MutationField::new(format!("level_{level:02}"), value).unwrap(),
            ])
            .unwrap()
        },
    );
    assert_eq!(deepest_reference.depth(), 59);
    let plan = sequence_plan(SequenceMutation::Insert {
        index: 0,
        value: deepest_reference,
    });
    let canonical = plan.canonical_json().unwrap();

    let mut json_budget = AssetLoadBudget::default();
    assert_eq!(
        MutationPlan::from_json_slice(&canonical, &mut json_budget).unwrap(),
        plan
    );
    assert_eq!(json_budget.usage().max_observed_depth, 186);
    let mut yaml_budget = AssetLoadBudget::default();
    assert_eq!(
        MutationPlan::from_yaml_slice(&canonical, &mut yaml_budget).unwrap(),
        plan
    );
    assert_eq!(yaml_budget.usage().max_observed_depth, 186);
}

#[test]
fn typed_readers_reject_wire_depth_187() {
    const MAX_WIRE_DEPTH: usize = 186;
    let nested = format!(
        "{}0{}",
        "[".repeat(MAX_WIRE_DEPTH + 1),
        "]".repeat(MAX_WIRE_DEPTH + 1)
    );

    assert!(matches!(
        MutationPlan::from_json_slice(nested.as_bytes(), &mut AssetLoadBudget::default()),
        Err(MutationPlanReadError::NestingDepthExceeded {
            format: "JSON",
            maximum: 186,
            actual: 187,
        })
    ));

    let yaml = format!("---\n{nested}\n");
    assert!(matches!(
        MutationPlan::from_yaml_slice(yaml.as_bytes(), &mut AssetLoadBudget::default()),
        Err(MutationPlanReadError::NestingDepthExceeded {
            format: "YAML",
            maximum: 186,
            actual: 187,
        })
    ));
}

#[test]
fn normalization_applies_only_to_set_like_plan_data() {
    let left = sample_plan();
    let mut sources = expectations();
    sources.reverse();

    let payload_a = PlanPayload::new(vec![1, 2, 3]);
    let payload_b = PlanPayload::new(vec![4, 5, 6]);
    let digest_a = payload_a.digest();
    let digest_b = payload_b.digest();
    let action_a = GenericMutation::ResourceReplace {
        target: binary_address(),
        path: FieldPath::root().push_field("a").unwrap(),
        guard: FieldGuard::new(digest(b"schema a"), digest(b"value a")),
        payload: digest_a,
    };
    let action_b = GenericMutation::ResourceReplace {
        target: binary_address(),
        path: FieldPath::root().push_field("b").unwrap(),
        guard: FieldGuard::new(digest(b"schema b"), digest(b"value b")),
        payload: digest_b,
    };
    let forward = MutationPlan::new(
        left.workspace_id(),
        left.base_revision(),
        vec![sources[0].clone(), sources[1].clone()],
        vec![payload_b.clone(), payload_a.clone()],
        vec![action_a.clone(), action_b.clone()],
    )
    .unwrap_err();
    assert!(matches!(
        forward,
        MutationPlanError::UnusedSourceExpectation(_)
    ));

    let source = expectations().remove(0);
    let forward = MutationPlan::new(
        left.workspace_id(),
        left.base_revision(),
        vec![source.clone(), source.clone()],
        vec![payload_b.clone(), payload_a.clone()],
        vec![action_a.clone(), action_b.clone()],
    )
    .unwrap();
    let reverse_collections = MutationPlan::new(
        left.workspace_id(),
        left.base_revision(),
        vec![source],
        vec![payload_a, payload_b],
        vec![action_a.clone(), action_b.clone()],
    )
    .unwrap();
    assert_eq!(
        forward.canonical_json().unwrap(),
        reverse_collections.canonical_json().unwrap()
    );

    let reverse_operations = MutationPlan::new(
        left.workspace_id(),
        left.base_revision(),
        vec![expectations().remove(0)],
        reverse_collections.payloads().to_vec(),
        vec![action_b, action_a.clone()],
    )
    .unwrap();
    assert_ne!(
        forward.canonical_json().unwrap(),
        reverse_operations.canonical_json().unwrap()
    );

    let duplicate_operations = MutationPlan::new(
        left.workspace_id(),
        left.base_revision(),
        vec![expectations().remove(0)],
        vec![PlanPayload::new(vec![1, 2, 3])],
        vec![action_a.clone(), action_a],
    )
    .unwrap();
    assert_eq!(duplicate_operations.operations().len(), 2);
    assert_eq!(duplicate_operations.operations()[1].ordinal(), 1);
}

#[test]
fn deserialization_rejects_non_consecutive_operation_ordinals() {
    let plan = sample_plan();
    let mut value = serde_json::to_value(&plan).unwrap();
    value["operations"][1]["ordinal"] = serde_json::json!(0);
    assert!(read_json_value(value).is_err());

    let mut value = serde_json::to_value(&plan).unwrap();
    value["operations"][0]["ordinal"] = serde_json::json!(1);
    assert!(read_json_value(value).is_err());
}

#[test]
fn source_and_payload_coverage_are_exact() {
    let action = GenericMutation::FieldReplace {
        target: binary_address(),
        path: FieldPath::root().push_field("m_Name").unwrap(),
        guard: FieldGuard::new(digest(b"schema"), digest(b"value")),
        replacement: MutationValue::string("name").unwrap(),
    };
    let revision = WorkspaceRevision::new(digest(b"revision"));

    assert!(matches!(
        MutationPlan::new(
            workspace_id(),
            revision,
            Vec::new(),
            Vec::new(),
            vec![action.clone()],
        ),
        Err(MutationPlanError::MissingSourceExpectation(_))
    ));
    assert!(matches!(
        MutationPlan::new(
            workspace_id(),
            revision,
            vec![SourceExpectation::new(
                binary_locator(),
                SourceFingerprint::from_bytes(SourceKind::Yaml, b"wrong kind"),
            )],
            Vec::new(),
            vec![action.clone()],
        ),
        Err(MutationPlanError::SourceKindMismatch { .. })
    ));
    assert!(matches!(
        MutationPlan::new(
            workspace_id(),
            revision,
            vec![expectations().remove(0)],
            vec![PlanPayload::new(vec![9])],
            vec![action],
        ),
        Err(MutationPlanError::UnusedPayload(_))
    ));

    let expected = expectations().remove(0);
    let conflicting = SourceExpectation::new(
        expected.locator().clone(),
        SourceFingerprint::from_bytes(SourceKind::SerializedFile, b"different bytes"),
    );
    assert!(matches!(
        MutationPlan::new(
            workspace_id(),
            revision,
            vec![expected, conflicting],
            Vec::new(),
            vec![GenericMutation::FieldReplace {
                target: binary_address(),
                path: FieldPath::root().push_field("m_Name").unwrap(),
                guard: FieldGuard::new(digest(b"schema"), digest(b"value")),
                replacement: MutationValue::null(),
            }],
        ),
        Err(MutationPlanError::ConflictingSourceExpectation { .. })
    ));
}

#[test]
fn mutation_value_depth_is_enforced_at_the_contract_boundary() {
    let nested = |arrays: usize| {
        (0..arrays).try_fold(MutationValue::null(), |value, _| {
            MutationValue::array(vec![value])
        })
    };
    let action = |replacement| GenericMutation::FieldReplace {
        target: binary_address(),
        path: FieldPath::root().push_field("nested").unwrap(),
        guard: FieldGuard::new(digest(b"schema"), digest(b"value")),
        replacement,
    };
    let revision = WorkspaceRevision::new(digest(b"revision"));

    let array_limit = MutationPlan::new(
        workspace_id(),
        revision,
        vec![expectations().remove(0)],
        Vec::new(),
        vec![action(nested(58).unwrap())],
    )
    .unwrap();
    let canonical = array_limit.canonical_json().unwrap();
    assert_eq!(read_json_plan(&canonical).unwrap(), array_limit);

    let deepest_object = (0..58).fold(
        MutationValue::object(Vec::new()).unwrap(),
        |value, level| {
            MutationValue::object(vec![
                MutationField::new(format!("level_{level:02}"), value).unwrap(),
            ])
            .unwrap()
        },
    );
    let object_limit = MutationPlan::new(
        workspace_id(),
        revision,
        vec![expectations().remove(0)],
        Vec::new(),
        vec![action(deepest_object)],
    )
    .unwrap();
    let canonical = object_limit.canonical_json().unwrap();
    let mut json_budget = AssetLoadBudget::default();
    assert_eq!(
        MutationPlan::from_json_slice(&canonical, &mut json_budget).unwrap(),
        object_limit
    );
    assert_eq!(json_budget.usage().max_observed_depth, 180);
    let mut yaml_budget = AssetLoadBudget::default();
    assert_eq!(
        MutationPlan::from_yaml_slice(&canonical, &mut yaml_budget).unwrap(),
        object_limit
    );
    assert_eq!(yaml_budget.usage().max_observed_depth, 180);

    assert!(matches!(
        nested(59),
        Err(MutationPlanError::ValueDepthExceeded {
            maximum: 59,
            actual: 60,
        })
    ));
}

#[test]
fn validated_mutation_values_remain_pattern_matchable() {
    let value = MutationValue::object(vec![
        MutationField::new(
            "nested",
            MutationValue::array(vec![MutationValue::string("value").unwrap()]).unwrap(),
        )
        .unwrap(),
    ])
    .unwrap();

    let MutationValueRef::Object(fields) = value.view() else {
        panic!("expected object mutation value");
    };
    let MutationValueRef::Array(values) = fields[0].value().view() else {
        panic!("expected array mutation value");
    };
    assert_eq!(values[0].view(), MutationValueRef::String("value"));
}

#[test]
fn field_operations_reject_the_whole_object_root() {
    let target = binary_address();
    let source = expectations().remove(0);
    let guard = FieldGuard::new(digest(b"schema"), digest(b"value"));
    let actions = [
        GenericMutation::FieldReplace {
            target: target.clone(),
            path: FieldPath::root(),
            guard,
            replacement: MutationValue::null(),
        },
        GenericMutation::ReferenceReplace {
            target: target.clone(),
            path: FieldPath::root(),
            schema_digest: digest(b"schema"),
            expected: ReferenceTarget::null(),
            replacement: ReferenceTarget::null(),
        },
        GenericMutation::ResourceReplace {
            target: target.clone(),
            path: FieldPath::root(),
            guard,
            payload: digest(b"payload"),
        },
        GenericMutation::SequenceEdit {
            target,
            path: FieldPath::root(),
            guard,
            edit: SequenceMutation::Clear,
        },
    ];

    for action in actions {
        assert!(matches!(
            MutationPlan::new(
                workspace_id(),
                WorkspaceRevision::new(digest(b"revision")),
                vec![source.clone()],
                Vec::new(),
                vec![action],
            ),
            Err(MutationPlanError::RootFieldPath { .. })
        ));
    }
}

#[test]
fn wire_contract_rejects_unknown_tags_and_corrupt_payloads() {
    let plan = sample_plan();

    let legacy = read_json_plan(include_bytes!("mutation_plan_v1_all_operations.json"));
    assert!(legacy.is_err(), "v1 mutation plans must be rejected");

    let mut previous_version = serde_json::to_value(&plan).unwrap();
    previous_version["version"] = serde_json::json!(2);
    assert!(matches!(
        read_json_value(previous_version),
        Err(MutationPlanReadError::Contract(
            MutationPlanError::UnsupportedVersion(2)
        ))
    ));

    let mut unknown_version = serde_json::to_value(&plan).unwrap();
    unknown_version["version"] = serde_json::json!(4);
    assert!(matches!(
        read_json_value(unknown_version),
        Err(MutationPlanReadError::Contract(
            MutationPlanError::UnsupportedVersion(4)
        ))
    ));

    let mut missing_workspace = serde_json::to_value(&plan).unwrap();
    missing_workspace
        .as_object_mut()
        .unwrap()
        .remove("workspace_id");
    assert!(matches!(
        read_json_value(missing_workspace),
        Err(MutationPlanReadError::Contract(
            MutationPlanError::MissingWorkspaceId
        ))
    ));

    let mut unknown_operation = serde_json::to_value(&plan).unwrap();
    unknown_operation["operations"][0]["action"]["kind"] = serde_json::json!("setter");
    assert!(read_json_value(unknown_operation).is_err());

    let mut corrupt_payload = serde_json::to_value(&plan).unwrap();
    corrupt_payload["payloads"][0]["bytes"] = serde_json::json!("00");
    assert!(read_json_value(corrupt_payload).is_err());

    let mut bare_object_id = serde_json::to_value(&plan).unwrap();
    bare_object_id["operations"][0]["action"]["target"] = serde_json::json!({
        "kind": "binary",
        "version": 1,
        "source": {
            "version": 1,
            "workspace": "00000000000000000000000000000001",
            "kind": "serialized_file",
            "local": "00000000000000000000000000000001"
        },
        "path_id": -7
    });
    assert!(read_json_value(bare_object_id).is_err());

    assert!(
        serde_json::from_str::<MutationValue>(r#"{"kind":"float64","bits":"800000000000000A"}"#)
            .is_err()
    );
    assert!(serde_json::from_str::<MutationValue>(r#"{"kind":"bytes","value":"0"}"#).is_err());
    assert!(serde_json::from_str::<MutationValue>(
        r#"{"kind":"object","fields":[{"name":"same","value":{"kind":"null"}},{"name":"same","value":{"kind":"null"}}]}"#,
    )
    .is_err());
}

#[test]
fn budgeted_json_is_fragmentation_invariant() {
    let bytes = sample_plan().canonical_json().unwrap();
    let mut contiguous_budget = AssetLoadBudget::default();
    let contiguous = MutationPlan::from_json_slice(&bytes, &mut contiguous_budget).unwrap();
    let mut fragmented_budget = AssetLoadBudget::default();
    let fragmented =
        MutationPlan::from_json_reader(OneByteReader::new(&bytes), &mut fragmented_budget).unwrap();

    assert_eq!(contiguous, fragmented);
    assert_eq!(contiguous_budget.usage(), fragmented_budget.usage());
    assert!(contiguous_budget.usage().bytes > u64::try_from(bytes.len()).unwrap() * 7);
}

#[test]
fn serialized_inputs_fail_through_the_caller_budget() {
    let bytes = sample_plan().canonical_json().unwrap();
    let required_bytes = expected_json_budget_bytes(&bytes);
    let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: required_bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    MutationPlan::from_json_slice(&bytes, &mut exact_budget).unwrap();
    assert_eq!(exact_budget.usage().bytes, required_bytes);

    let mut byte_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: required_bytes - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        MutationPlan::from_json_slice(&bytes, &mut byte_budget),
        Err(MutationPlanReadError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));

    let mut member_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_members: 3,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        MutationPlan::from_yaml_slice(b"---\n[1, 2, 3, 4]\n", &mut member_budget),
        Err(MutationPlanReadError::Budget(BudgetError::Exceeded {
            resource: "members",
            ..
        }))
    ));
}

#[test]
fn yaml_is_a_strict_one_way_adapter_to_the_same_canonical_plan() {
    let plan = sample_plan();
    let canonical = plan.canonical_json().unwrap();
    let mut budget = AssetLoadBudget::default();
    let from_yaml = MutationPlan::from_yaml_slice(&canonical, &mut budget).unwrap();

    assert_eq!(from_yaml, plan);
    assert_eq!(from_yaml.canonical_json().unwrap(), canonical);
    assert_eq!(from_yaml.digest().unwrap(), plan.digest().unwrap());
}

#[test]
fn yaml_rejects_graph_and_non_json_data_model_features() {
    for (input, expected) in [
        ("---\n*missing\n", "alias"),
        ("---\n&named value\n", "anchor"),
        ("---\n&named [value]\n", "anchor"),
        ("---\n!custom value\n", "tag"),
        ("---\n!custom [value]\n", "tag"),
        ("%YAML 1.2\n---\n{}\n", "directive"),
        (
            "%TAG !example! tag:example.com,2026:plan/\n---\n{}\n",
            "directive",
        ),
        ("...\n%YAML 1.2\n---\n{}\n", "structure"),
        (
            "...\n%TAG !example! tag:example.com,2026:plan/\n---\n{}\n",
            "structure",
        ),
        ("---\n{}\n---\n{}\n", "document"),
        ("---\na: 1\na: 2\n", "duplicate"),
        ("---\n? [a]\n: b\n", "complex"),
        ("---\n1: value\n", "string"),
        ("---\n.nan\n", "finite"),
    ] {
        let error =
            MutationPlan::from_yaml_slice(input.as_bytes(), &mut AssetLoadBudget::default())
                .unwrap_err();
        let message = error.to_string().to_ascii_lowercase();
        assert!(
            message.contains(expected),
            "expected {expected:?} in {message:?} for {input:?}"
        );
    }

    assert!(matches!(
        MutationPlan::from_yaml_slice(&[0xff], &mut AssetLoadBudget::default()),
        Err(MutationPlanReadError::InvalidUtf8 { .. })
    ));
}

struct OneByteReader<'a> {
    remaining: &'a [u8],
}

impl<'a> OneByteReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }
}

impl Read for OneByteReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining.is_empty() || buffer.is_empty() {
            return Ok(0);
        }
        buffer[0] = self.remaining[0];
        self.remaining = &self.remaining[1..];
        Ok(1)
    }
}
