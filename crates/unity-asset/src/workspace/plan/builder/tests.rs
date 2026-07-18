use super::super::{FieldGuard, MutationValue, ObjectGuard, PlanBytes};
use super::*;
use unity_asset_core::{SourceFingerprint, SourceKind};

fn revision() -> WorkspaceRevision {
    WorkspaceRevision::new(DigestV1::hash_bytes(b"plan-builder-tests"))
}

fn locator() -> SourceLocator {
    SourceLocator::path("scene.prefab").unwrap()
}

fn address(anchor: &str) -> ObjectAddress {
    ObjectAddress::yaml(locator(), anchor).unwrap()
}

fn source(bytes: &[u8]) -> SourceExpectation {
    SourceExpectation::new(
        locator(),
        SourceFingerprint::from_bytes(SourceKind::Yaml, bytes),
    )
}

fn path(fields: &[&str]) -> FieldPath {
    fields.iter().fold(FieldPath::root(), |path, field| {
        path.push_field(*field).unwrap()
    })
}

fn guard() -> FieldGuard {
    FieldGuard::new(
        DigestV1::hash_bytes(b"schema"),
        DigestV1::hash_bytes(b"value"),
    )
}

fn field_action(target: ObjectAddress, fields: &[&str]) -> GenericMutation {
    GenericMutation::FieldReplace {
        target,
        path: path(fields),
        guard: guard(),
        replacement: MutationValue::signed(1),
    }
}

fn fragment(
    source: SourceExpectation,
    payloads: Vec<PlanPayload>,
    actions: Vec<GenericMutation>,
) -> MutationPlanFragment {
    MutationPlanFragment::from_recipe(revision(), vec![source], payloads, actions).unwrap()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuilderLogicalState {
    sources: Vec<SourceExpectation>,
    payloads: Vec<PlanPayload>,
    actions: Vec<GenericMutation>,
    latest_source_hash_indices: HashMap<u64, usize>,
    previous_source_hash_indices: Vec<Option<usize>>,
    payload_indices: HashMap<DigestV1, usize>,
    target_any_writes: WriteSummaryIndex,
    whole_object_writes: WriteSummaryIndex,
    exact_path_writes: WriteSummaryIndex,
    descendant_prefix_writes: WriteSummaryIndex,
    prior_write_index_lookups: usize,
}

fn logical_state(builder: &MutationPlanBuilder) -> BuilderLogicalState {
    BuilderLogicalState {
        sources: builder.sources.clone(),
        payloads: builder.payloads.clone(),
        actions: builder.actions.clone(),
        latest_source_hash_indices: builder.latest_source_hash_indices.clone(),
        previous_source_hash_indices: builder.previous_source_hash_indices.clone(),
        payload_indices: builder.payload_indices.clone(),
        target_any_writes: builder.target_any_writes.clone(),
        whole_object_writes: builder.whole_object_writes.clone(),
        exact_path_writes: builder.exact_path_writes.clone(),
        descendant_prefix_writes: builder.descendant_prefix_writes.clone(),
        prior_write_index_lookups: builder.prior_write_index_lookups,
    }
}

#[test]
fn append_rejects_source_conflicts_without_mutating_the_builder() {
    let mut builder = MutationPlanBuilder::new(revision());
    builder
        .append(fragment(
            source(b"first"),
            Vec::new(),
            vec![field_action(address("1"), &["m_First"])],
        ))
        .unwrap();
    let before = logical_state(&builder);

    let result = builder.append(fragment(
        source(b"second"),
        Vec::new(),
        vec![field_action(address("2"), &["m_Second"])],
    ));
    assert!(matches!(
        result,
        Err(MutationPlanBuilderError::Plan(
            MutationPlanError::ConflictingSourceExpectation { .. }
        ))
    ));
    assert_eq!(logical_state(&builder), before);

    builder
        .append(fragment(
            source(b"first"),
            Vec::new(),
            vec![field_action(address("2"), &["m_Second"])],
        ))
        .unwrap();
    assert_eq!(builder.build().unwrap().operations().len(), 2);
}

#[test]
fn append_rejects_payload_conflicts_without_mutating_the_builder() {
    let digest = DigestV1::hash_bytes(b"declared payload");
    let first = PlanPayload {
        digest,
        bytes: PlanBytes::new(b"first bytes".to_vec()),
    };
    let second = PlanPayload {
        digest,
        bytes: PlanBytes::new(b"second bytes".to_vec()),
    };
    let resource = |target: ObjectAddress, field: &str| GenericMutation::ResourceReplace {
        target,
        path: path(&[field]),
        guard: guard(),
        payload: digest,
    };
    let mut builder = MutationPlanBuilder::new(revision());
    builder
        .append(fragment(
            source(b"same"),
            vec![first],
            vec![resource(address("1"), "m_First")],
        ))
        .unwrap();
    let before = logical_state(&builder);

    let result = builder.append(fragment(
        source(b"same"),
        vec![second],
        vec![resource(address("2"), "m_Second")],
    ));
    assert!(matches!(
        result,
        Err(MutationPlanBuilderError::Plan(
            MutationPlanError::ConflictingPayload(actual)
        )) if actual == digest
    ));
    assert_eq!(logical_state(&builder), before);
}

#[test]
fn builder_rejects_overlapping_recipe_writes_at_every_scope() {
    let target = address("1");
    let mut builder = MutationPlanBuilder::new(revision());
    let same_fragment = fragment(
        source(b"same"),
        Vec::new(),
        vec![
            field_action(target.clone(), &["m_Vector"]),
            field_action(target.clone(), &["m_Vector", "x"]),
        ],
    );
    assert!(matches!(
        builder.append(same_fragment),
        Err(MutationPlanBuilderError::OverlappingWrites {
            first_index: 0,
            second_index: 1,
            ..
        })
    ));
    assert!(builder.actions.is_empty());

    builder
        .append(fragment(
            source(b"same"),
            Vec::new(),
            vec![GenericMutation::SchemaReplace {
                target: target.clone(),
                guard: ObjectGuard::new(
                    DigestV1::hash_bytes(b"schema"),
                    DigestV1::hash_bytes(b"value"),
                ),
                replacement: MutationValue::null(),
            }],
        ))
        .unwrap();
    assert!(matches!(
        builder.append(fragment(
            source(b"same"),
            Vec::new(),
            vec![field_action(target, &["m_Other"])],
        )),
        Err(MutationPlanBuilderError::OverlappingWrites { .. })
    ));
}

#[test]
fn builder_allows_the_same_path_on_distinct_objects() {
    let mut builder = MutationPlanBuilder::new(revision());
    for anchor in ["1", "2"] {
        builder
            .append(fragment(
                source(b"same"),
                Vec::new(),
                vec![field_action(address(anchor), &["m_Name"])],
            ))
            .unwrap();
    }
    assert_eq!(builder.build().unwrap().operations().len(), 2);
}

#[test]
fn builder_rejects_cross_fragment_ancestor_and_descendant_paths() {
    let target = address("1");
    for (first, second) in [
        (["m_Vector"].as_slice(), ["m_Vector", "x"].as_slice()),
        (["m_Vector", "x"].as_slice(), ["m_Vector"].as_slice()),
    ] {
        let mut builder = MutationPlanBuilder::new(revision());
        builder
            .append(fragment(
                source(b"same"),
                Vec::new(),
                vec![field_action(target.clone(), first)],
            ))
            .unwrap();

        assert!(matches!(
            builder.append(fragment(
                source(b"same"),
                Vec::new(),
                vec![field_action(target.clone(), second)],
            )),
            Err(MutationPlanBuilderError::OverlappingWrites {
                first_index: 0,
                second_index: 1,
                ..
            })
        ));
    }
}

#[test]
fn builder_reports_the_earliest_overlapping_prior_write() {
    let target = address("1");
    let mut builder = MutationPlanBuilder::new(revision());
    builder
        .append(fragment(
            source(b"same"),
            Vec::new(),
            vec![
                field_action(target.clone(), &["m_First"]),
                field_action(target.clone(), &["m_Second"]),
            ],
        ))
        .unwrap();
    let before = logical_state(&builder);

    let result = builder.append(fragment(
        source(b"same"),
        Vec::new(),
        vec![GenericMutation::SchemaReplace {
            target,
            guard: ObjectGuard::new(
                DigestV1::hash_bytes(b"schema"),
                DigestV1::hash_bytes(b"value"),
            ),
            replacement: MutationValue::null(),
        }],
    ));

    assert!(matches!(
        result,
        Err(MutationPlanBuilderError::OverlappingWrites {
            first_index: 0,
            second_index: 2,
            ..
        })
    ));
    assert_eq!(logical_state(&builder), before);
}

#[test]
fn write_summary_hash_collisions_recheck_target_and_path() {
    const COLLISION_HASH: u64 = 7;

    let first_target = address("1");
    let second_target = address("2");
    let first_path = path(&["m_First"]);
    let second_path = path(&["m_Second"]);
    let actions = vec![
        field_action(first_target.clone(), &["m_First"]),
        field_action(second_target.clone(), &["m_Second"]),
    ];
    let mut summaries = WriteSummaryIndex::default();
    summaries.insert(COLLISION_HASH, 0, 1);
    summaries.insert(COLLISION_HASH, 1, 1);

    assert_eq!(
        find_target_summary(&actions, &summaries, COLLISION_HASH, &first_target),
        Some(0)
    );
    assert_eq!(
        find_exact_path_summary(
            &actions,
            &summaries,
            COLLISION_HASH,
            &first_target,
            first_path.segments(),
        ),
        Some(0)
    );
    assert_eq!(
        find_descendant_prefix_summary(
            &actions,
            &summaries,
            COLLISION_HASH,
            &second_target,
            second_path.segments(),
        ),
        Some(1)
    );
    assert_eq!(
        find_exact_path_summary(
            &actions,
            &summaries,
            COLLISION_HASH,
            &first_target,
            second_path.segments(),
        ),
        None
    );
}

#[test]
fn builder_uses_incremental_indexes_for_many_distinct_fragments() {
    const FRAGMENT_COUNT: usize = 10_000;

    let expected_source = source(b"same");
    let mut builder = MutationPlanBuilder::new(revision());
    for index in 0..FRAGMENT_COUNT {
        let payload = PlanPayload::new(index.to_le_bytes().to_vec());
        let digest = payload.digest();
        let target = address(&index.to_string());
        builder
            .append(fragment(
                expected_source.clone(),
                vec![payload],
                vec![GenericMutation::ResourceReplace {
                    target,
                    path: path(&["m_Resource"]),
                    guard: guard(),
                    payload: digest,
                }],
            ))
            .unwrap();
    }

    assert_eq!(builder.sources.len(), FRAGMENT_COUNT);
    assert_eq!(builder.payloads.len(), FRAGMENT_COUNT);
    assert_eq!(builder.actions.len(), FRAGMENT_COUNT);
    assert_eq!(builder.latest_source_hash_indices.len(), 1);
    assert_eq!(builder.previous_source_hash_indices.len(), FRAGMENT_COUNT);
    assert!(
        builder
            .previous_source_hash_indices
            .iter()
            .all(Option::is_none)
    );
    assert_eq!(builder.payload_indices.len(), FRAGMENT_COUNT);
    assert_eq!(builder.target_any_writes.summaries.len(), FRAGMENT_COUNT);
    assert!(builder.whole_object_writes.summaries.is_empty());
    assert_eq!(builder.exact_path_writes.summaries.len(), FRAGMENT_COUNT);
    assert_eq!(
        builder.descendant_prefix_writes.summaries.len(),
        FRAGMENT_COUNT
    );
    assert_eq!(
        builder.prior_write_index_lookups,
        FRAGMENT_COUNT.saturating_mul(2)
    );
}

#[test]
fn builder_indexes_many_same_target_sibling_paths_linearly() {
    const FRAGMENT_COUNT: usize = 10_000;

    let expected_source = source(b"same");
    let payload = PlanPayload::new(b"same payload".to_vec());
    let payload_digest = payload.digest();
    let target = address("1");
    let mut builder = MutationPlanBuilder::new(revision());
    for index in 0..FRAGMENT_COUNT {
        let field = format!("m_Field{index}");
        builder
            .append(fragment(
                expected_source.clone(),
                vec![payload.clone()],
                vec![GenericMutation::ResourceReplace {
                    target: target.clone(),
                    path: path(&[&field]),
                    guard: guard(),
                    payload: payload_digest,
                }],
            ))
            .unwrap();
    }

    assert_eq!(builder.sources.len(), FRAGMENT_COUNT);
    assert_eq!(builder.payloads.len(), FRAGMENT_COUNT);
    assert_eq!(builder.actions.len(), FRAGMENT_COUNT);
    assert_eq!(builder.latest_source_hash_indices.len(), 1);
    assert!(
        builder
            .previous_source_hash_indices
            .iter()
            .all(Option::is_none)
    );
    assert_eq!(builder.payload_indices.len(), 1);
    assert_eq!(builder.target_any_writes.summaries.len(), 1);
    assert!(builder.whole_object_writes.summaries.is_empty());
    assert_eq!(builder.exact_path_writes.summaries.len(), FRAGMENT_COUNT);
    assert_eq!(
        builder.descendant_prefix_writes.summaries.len(),
        FRAGMENT_COUNT
    );
    assert_eq!(
        builder.prior_write_index_lookups,
        FRAGMENT_COUNT.saturating_mul(2)
    );
}

#[test]
fn builder_allows_disjoint_sibling_and_sequence_element_paths() {
    let target = address("1");
    let mut item_zero = path(&["items"]);
    item_zero = item_zero.push_index(0).unwrap();
    let mut item_one = path(&["items"]);
    item_one = item_one.push_index(1).unwrap();
    let action = |path| GenericMutation::FieldReplace {
        target: target.clone(),
        path,
        guard: guard(),
        replacement: MutationValue::signed(1),
    };
    let fragment = fragment(
        source(b"same"),
        Vec::new(),
        vec![
            field_action(target.clone(), &["alpha"]),
            field_action(target.clone(), &["beta"]),
            action(item_zero),
            action(item_one),
        ],
    );

    let mut builder = MutationPlanBuilder::new(revision());
    builder.append(fragment).unwrap();
    assert_eq!(builder.build().unwrap().operations().len(), 4);
}
