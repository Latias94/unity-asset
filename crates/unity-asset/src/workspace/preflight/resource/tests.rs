use std::fs;
use std::io::Read as _;
use std::sync::Arc;

use indexmap::IndexMap;
use unity_asset_core::{
    AssetLoadLimits, DigestV1, FieldPath, ObjectId, SourceAlias, SourceFingerprint, SourceId,
    SourceKind, UnityClass, UnityValue, WorkspaceId, class_ids, semantic_value_digest,
    yaml_field_schema_digest, yaml_schema_digest,
};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactBuildError, ArtifactLimits,
    PreparedArtifactKind,
};
use unity_asset_write::resources::StreamedResourceFlags;

use super::super::yaml::YamlObjectCandidate;
use super::*;
use crate::workspace::source_catalog::{
    PhysicalOrigin, SourceCatalog, SourceDescriptor, SourceLocationKind,
};

fn source(kind: SourceKind, local: u128) -> SourceId {
    SourceId::new(WorkspaceId::from_u128(1).unwrap(), kind, local).unwrap()
}

fn path(name: &str) -> FieldPath {
    FieldPath::root().push_field(name).unwrap()
}

fn stream_data_class(size: u64) -> TestUnityClassCandidate {
    TestUnityClassCandidate::new(UnityClass::with_properties(
        class_ids::AUDIO_CLIP,
        "AudioClip".to_owned(),
        "1".to_owned(),
        IndexMap::from([(
            "m_StreamData".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("path".to_owned(), UnityValue::String("old.resS".to_owned())),
                ("offset".to_owned(), UnityValue::Unsigned(91)),
                ("size".to_owned(), UnityValue::Unsigned(size)),
                ("untouched".to_owned(), UnityValue::Bool(true)),
            ])),
        )]),
    ))
}

fn resource_class() -> TestUnityClassCandidate {
    TestUnityClassCandidate::new(UnityClass::with_properties(
        class_ids::AUDIO_CLIP,
        "AudioClip".to_owned(),
        "1".to_owned(),
        IndexMap::from([(
            "m_Resource".to_owned(),
            UnityValue::Object(IndexMap::from([
                (
                    "m_Source".to_owned(),
                    UnityValue::String("old.resource".to_owned()),
                ),
                ("m_Offset".to_owned(), UnityValue::Integer(12)),
                ("m_Size".to_owned(), UnityValue::Integer(34)),
            ])),
        )]),
    ))
}

fn provenance(class: &UnityClass) -> SchemaProvenance {
    let digest = yaml_schema_digest(class, &mut AssetLoadBudget::default()).unwrap();
    SchemaProvenance::yaml(class.class_id(), digest)
}

fn guard(class: &UnityClass, path: &FieldPath) -> FieldGuard {
    let value = class.value_at_path(path).unwrap();
    let mut budget = AssetLoadBudget::default();
    FieldGuard::new(
        yaml_field_schema_digest(class, path, value, &mut budget).unwrap(),
        semantic_value_digest(value, &mut budget).unwrap(),
    )
}

fn input<'payload>(ordinal: u32, bytes: &'payload [u8]) -> ResourcePayloadInput<'payload> {
    ResourcePayloadInput::new(ordinal, DigestV1::hash_bytes(bytes), bytes)
}

fn builder<'payload>(
    parent: SourceId,
    payloads: &[ResourcePayloadInput<'payload>],
) -> ResourceSidecarBuilder<'payload> {
    ResourceSidecarBuilder::content_addressed(
        ResourceSidecarLocation::Companion { parent },
        StreamedResourceFlags::default(),
        None,
        "CAB.resS",
        payloads.len(),
        payloads.iter().copied(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap()
}

fn resource_fields<'class>(
    class: &'class UnityClass,
    field: &str,
) -> &'class IndexMap<String, UnityValue> {
    class.get(field).unwrap().as_object().unwrap()
}

fn yaml_candidate(class: &UnityClass) -> YamlObjectCandidate {
    let object =
        ObjectId::yaml(source(SourceKind::Yaml, 2), class.anchor().parse().unwrap()).unwrap();
    YamlObjectCandidate::from_class(
        object,
        0,
        Arc::new(provenance(class)),
        class,
        &mut AssetLoadBudget::default(),
    )
    .unwrap()
}

#[test]
fn target_wire_path_is_independent_from_sidecar_member_identity() {
    let payloads = [input(0, b"payload")];
    let builder = builder(source(SourceKind::SerializedFile, 1), &payloads);
    let member_name = builder.member_name().to_owned();
    let class = stream_data_class(4);
    let stream_path = path("m_StreamData");
    let current = class.value_at_path(&stream_path).unwrap();
    let preview = builder.preview_next(0, payloads[0].digest()).unwrap();
    let wire_path = format!("archive:/CAB-target/{member_name}");

    let staged = builder
        .stage_preview_with_wire_path(
            &preview,
            &stream_path,
            current,
            &wire_path,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(builder.member_name(), member_name);
    assert_eq!(
        staged
            .as_object()
            .unwrap()
            .get("path")
            .and_then(UnityValue::as_str),
        Some(wire_path.as_str())
    );
}

#[test]
fn consecutive_replacements_read_the_staged_value_and_append_contiguously() {
    let payloads = [input(3, b"abc"), input(11, b"de")];
    let mut builder = builder(source(SourceKind::SerializedFile, 1), &payloads);
    let member_name = builder.member_name().to_owned();
    let mut class = stream_data_class(4);
    let schema = provenance(&class);
    let stream_path = path("m_StreamData");
    let stale_guard = guard(&class, &stream_path);

    let first = builder
        .apply(
            3,
            payloads[0].digest(),
            &stream_path,
            stale_guard,
            &schema,
            &mut class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(first.offset(), 0);
    assert_eq!(first.size(), 3);
    let after_first = resource_fields(&class, "m_StreamData");
    assert_eq!(
        after_first.get("path"),
        Some(&UnityValue::String(member_name.clone()))
    );
    assert_eq!(after_first.get("offset"), Some(&UnityValue::Unsigned(0)));
    assert_eq!(after_first.get("size"), Some(&UnityValue::Unsigned(3)));
    assert_eq!(after_first.get("untouched"), Some(&UnityValue::Bool(true)));

    // Guard validation runs after previewing the second placement. A stale guard must not append
    // the previewed extent or alter the first staged value.
    let before_rejection = class.clone();
    let error = builder
        .apply(
            11,
            payloads[1].digest(),
            &stream_path,
            stale_guard,
            &schema,
            &mut class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ResourceReplaceError::FieldValueGuardMismatch { ordinal: 11, .. }
    ));
    assert_eq!(builder.extent_count(), 1);
    assert_eq!(class.properties(), before_rejection.properties());

    let current_guard = guard(&class, &stream_path);
    let second = builder
        .apply(
            11,
            payloads[1].digest(),
            &stream_path,
            current_guard,
            &schema,
            &mut class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(second.offset(), 3);
    assert_eq!(second.size(), 2);
    assert_eq!(builder.extent_count(), 2);
    let after_second = resource_fields(&class, "m_StreamData");
    assert_eq!(after_second.get("offset"), Some(&UnityValue::Unsigned(3)));
    assert_eq!(after_second.get("size"), Some(&UnityValue::Unsigned(2)));

    let manifest_digest = builder.manifest_digest();
    let finished = builder.finish(&mut AssetLoadBudget::default()).unwrap();
    assert_eq!(
        finished.catalog_update().fingerprint(),
        SourceFingerprint::from_bytes(SourceKind::StreamedResource, b"abcde")
    );
    assert_ne!(
        manifest_digest,
        finished.catalog_update().fingerprint().digest()
    );
}

#[test]
fn stale_resource_preview_cannot_replay_after_the_builder_advances() {
    let payloads = [input(1, b"a"), input(2, b"b")];
    let mut builder = builder(source(SourceKind::SerializedFile, 1), &payloads);
    let stale = builder
        .preview_next(payloads[0].ordinal, payloads[0].digest())
        .unwrap();
    let current = builder
        .preview_next(payloads[0].ordinal, payloads[0].digest())
        .unwrap();
    let target_path = path("m_StreamData");
    let mut first_candidate = stream_data_class(0);

    builder
        .commit_prepared(
            current,
            PreparedUnityFieldReplace {
                candidate: &mut first_candidate,
                path: &target_path,
                replacement: UnityValue::Unsigned(1),
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(
        first_candidate.class().value_at_path(&target_path),
        Ok(&UnityValue::Unsigned(1))
    );
    assert_eq!(builder.extent_count(), 1);

    let mut replay_candidate = stream_data_class(0);
    let replay_before = replay_candidate
        .class()
        .value_at_path(&target_path)
        .unwrap()
        .clone();
    let mut budget = AssetLoadBudget::default();
    let before = budget.usage();
    let error = builder
        .commit_prepared(
            stale,
            PreparedUnityFieldReplace {
                candidate: &mut replay_candidate,
                path: &target_path,
                replacement: UnityValue::Unsigned(2),
            },
            &mut budget,
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ResourceReplaceError::StalePreview {
            ordinal: 1,
            expected_index: 0,
            actual_index: 1,
        }
    ));
    assert_eq!(
        replay_candidate.class().value_at_path(&target_path),
        Ok(&replay_before)
    );
    assert_eq!(builder.extent_count(), 1);
    assert_eq!(budget.usage(), before);
}

#[test]
fn prepared_yaml_field_commits_only_after_the_previewed_extent_succeeds() {
    let payloads = [input(7, b"resource")];
    let mut builder = builder(source(SourceKind::Yaml, 1), &payloads);
    let mut candidate = yaml_candidate(&stream_data_class(0));
    let stream_path = path("m_StreamData");
    let original = candidate
        .class()
        .value_at_path(&stream_path)
        .unwrap()
        .clone();

    let preview = builder.preview_next(7, payloads[0].digest()).unwrap();
    let replacement = builder
        .stage_preview(
            &preview,
            &stream_path,
            candidate.class().value_at_path(&stream_path).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let field_guard = guard(candidate.class(), &stream_path);
    let validated = candidate
        .validate_replace_field_guard(
            7,
            stream_path.clone(),
            field_guard,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared = candidate
        .prepare_validated_replace_field(validated, replacement)
        .unwrap();
    let mut rejected_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: 1,
        max_bytes: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(
        builder
            .commit_prepared(preview, prepared, &mut rejected_budget)
            .is_err()
    );
    assert_eq!(builder.extent_count(), 0);
    assert_eq!(
        candidate.class().value_at_path(&stream_path).unwrap(),
        &original
    );

    let preview = builder.preview_next(7, payloads[0].digest()).unwrap();
    assert_eq!(preview.allocation().offset(), 0);
    let replacement = builder
        .stage_preview(
            &preview,
            &stream_path,
            candidate.class().value_at_path(&stream_path).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let guard = guard(candidate.class(), &stream_path);
    let validated = candidate
        .validate_replace_field_guard(
            7,
            stream_path.clone(),
            guard,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared = candidate
        .prepare_validated_replace_field(validated, replacement)
        .unwrap();
    builder
        .commit_prepared(preview, prepared, &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(builder.extent_count(), 1);
    let fields = resource_fields(candidate.class(), "m_StreamData");
    assert_eq!(fields.get("offset"), Some(&UnityValue::Unsigned(0)));
    assert_eq!(fields.get("size"), Some(&UnityValue::Unsigned(8)));
    assert_eq!(
        fields.get("path"),
        Some(&UnityValue::String(builder.member_name().to_owned()))
    );
}

#[test]
fn resource_shape_preserves_signed_integer_fields() {
    let payloads = [input(7, b"payload")];
    let mut builder = builder(source(SourceKind::SerializedFile, 1), &payloads);
    let mut class = resource_class();
    let schema = provenance(&class);
    let resource_path = path("m_Resource");
    let allocation = builder
        .apply(
            7,
            payloads[0].digest(),
            &resource_path,
            guard(&class, &resource_path),
            &schema,
            &mut class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(allocation.offset(), 0);
    let fields = resource_fields(&class, "m_Resource");
    assert_eq!(
        fields.get("m_Source"),
        Some(&UnityValue::String(builder.member_name().to_owned()))
    );
    assert_eq!(fields.get("m_Offset"), Some(&UnityValue::Integer(0)));
    assert_eq!(fields.get("m_Size"), Some(&UnityValue::Integer(7)));
    builder.finish(&mut AssetLoadBudget::default()).unwrap();
}

#[test]
fn builder_and_apply_accept_exact_budget_and_reject_one_short_atomically() {
    let payloads = [input(2, b"abc"), input(9, b"defg")];
    let parent = source(SourceKind::SerializedFile, 1);

    let mut measured = AssetLoadBudget::default();
    let _measured_builder = ResourceSidecarBuilder::content_addressed(
        ResourceSidecarLocation::Companion { parent },
        StreamedResourceFlags::default(),
        None,
        "CAB.resS",
        payloads.len(),
        payloads.iter().copied(),
        &mut measured,
    )
    .unwrap();
    let construction_usage = measured.usage();
    let exact_limits = AssetLoadLimits {
        max_entries: construction_usage.entries,
        max_bytes: construction_usage.bytes,
        ..AssetLoadLimits::default()
    };
    let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
    let _exact_builder = ResourceSidecarBuilder::content_addressed(
        ResourceSidecarLocation::Companion { parent },
        StreamedResourceFlags::default(),
        None,
        "CAB.resS",
        payloads.len(),
        payloads.iter().copied(),
        &mut exact,
    )
    .unwrap();
    assert_eq!(exact.usage(), construction_usage);

    let one_short_limits = AssetLoadLimits {
        max_entries: construction_usage.entries,
        max_bytes: construction_usage.bytes - 1,
        ..AssetLoadLimits::default()
    };
    let mut one_short = AssetLoadBudget::new(one_short_limits).unwrap();
    assert!(matches!(
        ResourceSidecarBuilder::content_addressed(
            ResourceSidecarLocation::Companion { parent },
            StreamedResourceFlags::default(),
            None,
            "CAB.resS",
            payloads.len(),
            payloads.iter().copied(),
            &mut one_short,
        ),
        Err(ResourceSidecarBuildError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));
    let failed_usage = one_short.usage();
    assert_eq!(failed_usage.entries, construction_usage.entries);
    assert!(failed_usage.bytes > 0);
    assert!(failed_usage.bytes < construction_usage.bytes);

    let single = [input(2, b"abc")];
    let mut measured_builder = builder(parent, &single);
    let mut measured_class = stream_data_class(0);
    let measured_schema = provenance(&measured_class);
    let stream_path = path("m_StreamData");
    let measured_guard = guard(&measured_class, &stream_path);
    let mut measured_apply = AssetLoadBudget::default();
    measured_builder
        .apply(
            2,
            single[0].digest(),
            &stream_path,
            measured_guard,
            &measured_schema,
            &mut measured_class,
            &mut measured_apply,
        )
        .unwrap();
    let apply_usage = measured_apply.usage();

    let mut exact_builder = builder(parent, &single);
    let mut exact_class = stream_data_class(0);
    let exact_schema = provenance(&exact_class);
    let exact_guard = guard(&exact_class, &stream_path);
    let mut exact_apply = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: apply_usage.entries,
        max_bytes: apply_usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    exact_builder
        .apply(
            2,
            single[0].digest(),
            &stream_path,
            exact_guard,
            &exact_schema,
            &mut exact_class,
            &mut exact_apply,
        )
        .unwrap();
    assert_eq!(exact_apply.usage().bytes, apply_usage.bytes);
    assert_eq!(exact_apply.usage().entries, apply_usage.entries);

    let mut rejected_builder = builder(parent, &single);
    let mut rejected_class = stream_data_class(0);
    let rejected_before = rejected_class.clone();
    let rejected_schema = provenance(&rejected_class);
    let rejected_guard = guard(&rejected_class, &stream_path);
    let mut rejected_apply = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: apply_usage.entries,
        max_bytes: apply_usage.bytes - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(
        rejected_builder
            .apply(
                2,
                single[0].digest(),
                &stream_path,
                rejected_guard,
                &rejected_schema,
                &mut rejected_class,
                &mut rejected_apply,
            )
            .is_err()
    );
    assert_eq!(rejected_builder.extent_count(), 0);
    assert_eq!(rejected_class.properties(), rejected_before.properties());
}

#[test]
fn payload_scan_budget_is_linear_and_rejected_before_content_is_read() {
    const LARGE_PAYLOAD_BYTES: usize = 64 * 1024;

    let parent = source(SourceKind::SerializedFile, 1);
    let small_bytes = [0x5a];
    let large_bytes = vec![0x5a; LARGE_PAYLOAD_BYTES];
    let small_payloads = [input(2, &small_bytes)];
    let large_payloads = [input(2, &large_bytes)];

    let mut small_construction = AssetLoadBudget::default();
    let mut small_builder = ResourceSidecarBuilder::content_addressed(
        ResourceSidecarLocation::Companion { parent },
        StreamedResourceFlags::default(),
        None,
        "CAB.resS",
        small_payloads.len(),
        small_payloads,
        &mut small_construction,
    )
    .unwrap();
    let mut small_class = stream_data_class(0);
    let small_schema = provenance(&small_class);
    let stream_path = path("m_StreamData");
    small_builder
        .apply(
            2,
            small_payloads[0].digest(),
            &stream_path,
            guard(&small_class, &stream_path),
            &small_schema,
            &mut small_class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let mut large_construction = AssetLoadBudget::default();
    let mut large_builder = ResourceSidecarBuilder::content_addressed(
        ResourceSidecarLocation::Companion { parent },
        StreamedResourceFlags::default(),
        None,
        "CAB.resS",
        large_payloads.len(),
        large_payloads,
        &mut large_construction,
    )
    .unwrap();
    assert_eq!(large_construction.usage(), small_construction.usage());
    let mut large_class = stream_data_class(0);
    let large_schema = provenance(&large_class);
    large_builder
        .apply(
            2,
            large_payloads[0].digest(),
            &stream_path,
            guard(&large_class, &stream_path),
            &large_schema,
            &mut large_class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let mut small_finish = AssetLoadBudget::default();
    small_builder.finish(&mut small_finish).unwrap();
    let small_usage = small_finish.usage();
    let mut large_finish = AssetLoadBudget::default();
    large_builder.finish(&mut large_finish).unwrap();
    let large_usage = large_finish.usage();
    assert_eq!(large_usage.entries, small_usage.entries);
    assert_eq!(
        large_usage.bytes - small_usage.bytes,
        u64::try_from(LARGE_PAYLOAD_BYTES - small_bytes.len()).unwrap()
    );

    let mut exact_builder = builder(parent, &large_payloads);
    let mut exact_class = stream_data_class(0);
    let exact_schema = provenance(&exact_class);
    exact_builder
        .apply(
            2,
            large_payloads[0].digest(),
            &stream_path,
            guard(&exact_class, &stream_path),
            &exact_schema,
            &mut exact_class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut exact_finish = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: large_usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    exact_builder.finish(&mut exact_finish).unwrap();
    assert_eq!(exact_finish.usage(), large_usage);

    let mut rejected_builder = builder(parent, &large_payloads);
    let mut rejected_class = stream_data_class(0);
    let rejected_schema = provenance(&rejected_class);
    rejected_builder
        .apply(
            2,
            large_payloads[0].digest(),
            &stream_path,
            guard(&rejected_class, &stream_path),
            &rejected_schema,
            &mut rejected_class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let one_short_limit = large_usage.bytes - 1;
    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: one_short_limit,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let error = match rejected_builder.finish(&mut one_short) {
        Ok(_) => panic!("one-short content budget must reject sidecar finish"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ResourceSidecarFinishError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit,
            requested,
        }) if limit == one_short_limit && requested == large_usage.bytes
    ));
    assert_eq!(one_short.usage().bytes, 0);
}

#[test]
fn multi_payload_finish_scans_the_concatenated_content_once_with_exact_budget() {
    let parent = source(SourceKind::SerializedFile, 1);
    let payloads = [input(2, b"abc"), input(9, b"defg")];
    let stream_path = path("m_StreamData");

    let complete = |builder: &mut ResourceSidecarBuilder<'_>| {
        for payload in payloads {
            let mut class = stream_data_class(0);
            let schema = provenance(&class);
            builder
                .apply(
                    payload.ordinal,
                    payload.digest(),
                    &stream_path,
                    guard(&class, &stream_path),
                    &schema,
                    &mut class,
                    &mut AssetLoadBudget::default(),
                )
                .unwrap();
        }
    };

    let mut measured_builder = builder(parent, &payloads);
    complete(&mut measured_builder);
    let mut measured = AssetLoadBudget::default();
    let finished = measured_builder.finish(&mut measured).unwrap();
    assert_eq!(measured.usage().bytes, 7);
    assert_eq!(
        finished.catalog_update().fingerprint(),
        SourceFingerprint::from_bytes(SourceKind::StreamedResource, b"abcdefg")
    );

    let mut exact_builder = builder(parent, &payloads);
    complete(&mut exact_builder);
    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured.usage().bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    exact_builder.finish(&mut exact).unwrap();
    assert_eq!(exact.usage(), measured.usage());

    let mut rejected_builder = builder(parent, &payloads);
    complete(&mut rejected_builder);
    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured.usage().bytes - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        rejected_builder.finish(&mut one_short),
        Err(ResourceSidecarFinishError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit: 6,
            requested: 7,
        }))
    ));
    assert_eq!(one_short.usage().bytes, 0);
}

#[test]
fn stream_data_u32_domain_is_enforced_without_committing_a_preview() {
    assert!(matches!(
        checked_stream_data_size(17, u64::from(u32::MAX) + 1),
        Err(ResourceReplaceError::StreamDataSizeOverflow {
            ordinal: 17,
            size
        }) if size == u64::from(u32::MAX) + 1
    ));

    let payloads = [input(17, b"x")];
    let mut builder = builder(source(SourceKind::SerializedFile, 1), &payloads);
    let mut class = stream_data_class(u64::from(u32::MAX) + 1);
    let before = class.clone();
    let schema = provenance(&class);
    let stream_path = path("m_StreamData");
    let error = builder
        .apply(
            17,
            payloads[0].digest(),
            &stream_path,
            guard(&class, &stream_path),
            &schema,
            &mut class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ResourceReplaceError::ExistingStreamDataSizeOverflow { ordinal: 17, .. }
    ));
    assert_eq!(builder.extent_count(), 0);
    assert_eq!(class.properties(), before.properties());
}

#[test]
fn finish_rejects_missing_payloads_and_manifest_identity_is_deterministic() {
    let first_inputs = [input(4, b"a"), input(8, b"bc")];
    let second_inputs = [input(4, b"a"), input(8, b"bc")];
    let parent = source(SourceKind::SerializedFile, 1);
    let first = builder(parent, &first_inputs);
    let second = builder(parent, &second_inputs);
    assert_eq!(first.member_name(), second.member_name());
    assert_eq!(first.manifest_digest(), second.manifest_digest());
    assert_eq!(first.expected_count(), 2);
    assert!(matches!(
        first.finish(&mut AssetLoadBudget::default()),
        Err(ResourceSidecarFinishError::Incomplete {
            expected: 2,
            applied: 0
        })
    ));

    let single = [input(99, b"same")];
    let single_builder = builder(parent, &single);
    assert_eq!(single_builder.manifest_digest(), single[0].digest());

    let different_ordinals = [input(5, b"a"), input(8, b"bc")];
    let different = builder(parent, &different_ordinals);
    assert_ne!(second.manifest_digest(), different.manifest_digest());

    let reversed = [input(8, b"bc"), input(4, b"a")];
    assert!(matches!(
        ResourceSidecarBuilder::content_addressed(
            ResourceSidecarLocation::Companion { parent },
            StreamedResourceFlags::default(),
            None,
            "CAB.resS",
            reversed.len(),
            reversed.iter().copied(),
            &mut AssetLoadBudget::default(),
        ),
        Err(ResourceSidecarBuildError::OperationOrder {
            ordinal: 4,
            previous: 8
        })
    ));
}

#[test]
fn sidecar_names_reuse_validated_directory_utf8_and_portability_rules() {
    let payloads = [input(1, b"resource")];
    let parent = source(SourceKind::SerializedFile, 1);
    let directory = LogicalArtifactName::new("build/main.assets_data").unwrap();
    let long_utf8_base = format!("{}.resS", "资源".repeat(200));
    let expected =
        LogicalArtifactName::sidecar(Some(&directory), &long_utf8_base, payloads[0].digest())
            .unwrap();
    let builder = ResourceSidecarBuilder::content_addressed(
        ResourceSidecarLocation::Companion { parent },
        StreamedResourceFlags::default(),
        Some(&directory),
        &long_utf8_base,
        1,
        payloads,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    assert_eq!(builder.member_name(), expected.as_str());
    assert!(builder.member_name().starts_with("build/main.assets_data/"));
    assert!(
        builder
            .member_name()
            .is_char_boundary(builder.member_name().len())
    );

    let upper_directory = LogicalArtifactName::new("Data").unwrap();
    let lower_directory = LogicalArtifactName::new("data").unwrap();
    let mut upper = ResourceSidecarBuilder::content_addressed(
        ResourceSidecarLocation::Companion { parent },
        StreamedResourceFlags::default(),
        Some(&upper_directory),
        "CAB.resS",
        1,
        payloads,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let mut lower = ResourceSidecarBuilder::content_addressed(
        ResourceSidecarLocation::Companion { parent },
        StreamedResourceFlags::default(),
        Some(&lower_directory),
        "CAB.resS",
        1,
        payloads,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let stream_path = path("m_StreamData");
    let mut upper_class = stream_data_class(0);
    let upper_schema = provenance(&upper_class);
    upper
        .apply(
            1,
            payloads[0].digest(),
            &stream_path,
            guard(&upper_class, &stream_path),
            &upper_schema,
            &mut upper_class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut lower_class = stream_data_class(0);
    let lower_schema = provenance(&lower_class);
    lower
        .apply(
            1,
            payloads[0].digest(),
            &stream_path,
            guard(&lower_class, &stream_path),
            &lower_schema,
            &mut lower_class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let _upper = upper
        .finish(&mut AssetLoadBudget::default())
        .unwrap()
        .declare(&mut declaration)
        .unwrap();
    let _lower = lower
        .finish(&mut AssetLoadBudget::default())
        .unwrap()
        .declare(&mut declaration)
        .unwrap();
    assert!(matches!(
        declaration.seal_output_names(),
        Err(ArtifactBuildError::Name(
            ArtifactNameError::PortabilityCollision { .. }
        ))
    ));
}

#[test]
fn companion_sidecar_prepares_one_exact_output_without_filesystem_writes() {
    let payloads = [input(1, b"abc"), input(2, b"de")];
    let mut builder = builder(source(SourceKind::SerializedFile, 1), &payloads);
    let mut first_class = stream_data_class(0);
    let mut second_class = stream_data_class(0);
    let first_schema = provenance(&first_class);
    let second_schema = provenance(&second_class);
    let stream_path = path("m_StreamData");
    builder
        .apply(
            1,
            payloads[0].digest(),
            &stream_path,
            guard(&first_class, &stream_path),
            &first_schema,
            &mut first_class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    builder
        .apply(
            2,
            payloads[1].digest(),
            &stream_path,
            guard(&second_class, &stream_path),
            &second_schema,
            &mut second_class,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let finished = builder.finish(&mut AssetLoadBudget::default()).unwrap();
    let expected_name = finished.catalog_update().member_name().to_owned();

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let declared = finished.declare(&mut declaration).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let prepared = declared.prepare(&mut batch).unwrap();
    let handle = prepared.artifact();
    assert!(prepared.catalog_update().location().publication_root());
    let (_, update) = prepared.into_parts();
    assert_eq!(
        update.fingerprint(),
        SourceFingerprint::from_bytes(SourceKind::StreamedResource, b"abcde")
    );
    let artifacts = batch.finish().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts.proof_image_count(), 1);
    let output = artifacts.outputs().next().unwrap();
    assert_eq!(output.name().as_str(), expected_name);
    assert_eq!(output.handle(), handle);
    let artifact = artifacts.artifact(handle).unwrap();
    assert_eq!(artifact.kind(), PreparedArtifactKind::StreamedResource);
    assert_eq!(artifact.len(), 5);
    assert_eq!(artifact.digest(), update.fingerprint().digest());
    let mut actual = Vec::new();
    artifact.reader().read_to_end(&mut actual).unwrap();
    assert_eq!(actual, b"abcde");
}

#[test]
fn catalog_updates_distinguish_companion_and_contained_sidecars() {
    for (parent_kind, expected_location) in [
        (SourceKind::SerializedFile, SourceLocationKind::Companion),
        (SourceKind::AssetBundle, SourceLocationKind::Sidecar),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root_path = directory.path().join(match parent_kind {
            SourceKind::SerializedFile => "main.assets",
            SourceKind::AssetBundle => "main.bundle",
            _ => unreachable!(),
        });
        fs::write(&root_path, b"baseline").unwrap();
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut catalog = SourceCatalog::new(workspace);
        let parent = catalog
            .register(
                SourceDescriptor::root(
                    parent_kind,
                    SourceAlias::new(root_path.file_name().unwrap().to_string_lossy().as_ref())
                        .unwrap(),
                    PhysicalOrigin::from_existing_path(&root_path).unwrap(),
                ),
                SourceFingerprint::from_bytes(parent_kind, b"baseline"),
            )
            .unwrap();
        let location = match parent_kind {
            SourceKind::SerializedFile => ResourceSidecarLocation::Companion { parent },
            SourceKind::AssetBundle => ResourceSidecarLocation::Contained { container: parent },
            _ => unreachable!(),
        };
        let payloads = [input(1, b"resource")];
        let mut builder = ResourceSidecarBuilder::content_addressed(
            location,
            StreamedResourceFlags::default(),
            None,
            "CAB.resS",
            1,
            payloads,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let mut class = stream_data_class(0);
        let schema = provenance(&class);
        let stream_path = path("m_StreamData");
        builder
            .apply(
                1,
                payloads[0].digest(),
                &stream_path,
                guard(&class, &stream_path),
                &schema,
                &mut class,
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let (_plan, update) = builder
            .finish(&mut AssetLoadBudget::default())
            .unwrap()
            .into_parts();
        assert_eq!(update.location(), location);
        assert_eq!(
            update.location().publication_root(),
            expected_location == SourceLocationKind::Companion
        );

        let mut begin_budget = AssetLoadBudget::default();
        let mut transaction = catalog.begin_transaction(&mut begin_budget).unwrap();
        let mut operation_budget = AssetLoadBudget::default();
        let sidecar = update
            .apply(&mut transaction, &mut operation_budget)
            .unwrap();
        let candidate = transaction.commit(&mut operation_budget).unwrap();
        assert_eq!(
            candidate.resolve(sidecar).unwrap().location_kind(),
            expected_location
        );
        assert_eq!(candidate.parent(sidecar).unwrap(), Some(parent));
        assert_eq!(
            candidate.fingerprint(sidecar).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::StreamedResource, b"resource")
        );
    }
}
