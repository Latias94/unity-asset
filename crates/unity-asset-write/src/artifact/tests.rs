use std::io::{Read, Write};
use std::sync::Arc;

use unity_asset_binary::reader::ByteOrder;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadUsage, DigestV1, SourceId, SourceKind, VerifiedSourceImage,
    WorkspaceId, vec_allocation_bytes,
};

use super::*;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn name(value: &str) -> LogicalArtifactName {
    LogicalArtifactName::new(value).unwrap()
}

fn source_payload(bytes: Vec<u8>, kind: SourceKind, local: u128) -> ArtifactPayload {
    let source = SourceId::new(WorkspaceId::from_u128(17).unwrap(), kind, local).unwrap();
    let image = VerifiedSourceImage::verify(kind, Arc::<[u8]>::from(bytes));
    ArtifactPayload::source_backed(source, image).unwrap()
}

fn serialized_payload(local: u128) -> ArtifactPayload {
    source_payload(
        minimal_serialized_file_v8_le(),
        SourceKind::SerializedFile,
        local,
    )
}

fn resource_payload(bytes: &[u8], local: u128) -> ArtifactPayload {
    source_payload(bytes.to_vec(), SourceKind::StreamedResource, local)
}

fn yaml_payload(bytes: &[u8], local: u128) -> ArtifactPayload {
    source_payload(bytes.to_vec(), SourceKind::Yaml, local)
}

fn minimal_serialized_file_v8_le() -> Vec<u8> {
    let version = 8_u32;
    let data_offset = 128_u32;
    let mut metadata = Vec::new();
    metadata.extend_from_slice(b"2.5.0f5\0");
    metadata.extend_from_slice(&0_i32.to_le_bytes());
    metadata.extend_from_slice(&0_i32.to_le_bytes());
    metadata.extend_from_slice(&0_i32.to_le_bytes());
    metadata.extend_from_slice(&0_i32.to_le_bytes());
    metadata.extend_from_slice(&0_i32.to_le_bytes());
    metadata.push(0);

    let metadata_size = u32::try_from(metadata.len() + 1).unwrap();
    let file_size = data_offset.checked_add(metadata_size).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&metadata_size.to_be_bytes());
    bytes.extend_from_slice(&file_size.to_be_bytes());
    bytes.extend_from_slice(&version.to_be_bytes());
    bytes.extend_from_slice(&data_offset.to_be_bytes());
    bytes.resize(data_offset as usize, 0);
    bytes.push(0);
    bytes.extend_from_slice(&metadata);
    bytes
}

fn prepare_resource(
    batch: &mut ArtifactBatch<'_, '_>,
    payload: &ArtifactPayload,
) -> Result<ArtifactHandle, ArtifactBuildError> {
    batch.prepare_streamed_resource_extents([resource_extent(payload, 16)], |encoder| {
        encoder.push_payload_full(payload)
    })
}

fn prepare_serialized_set(
    payload: &ArtifactPayload,
    budget: &mut ArtifactBudget,
) -> Result<PreparedArtifactSet, ArtifactBuildError> {
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration = ArtifactBatchDeclaration::begin(budget, &mut inspection_budget)?;
    let output = declaration.declare_output(name("main.assets"))?;
    let mut batch = declaration.seal_output_names()?;
    let artifact = batch
        .prepare_serialized_file(payload.len(), |encoder| encoder.push_payload_full(payload))?;
    batch.bind_output(output, artifact)?;
    batch.finish()
}

fn resource_extent(payload: &ArtifactPayload, alignment: u32) -> StreamedResourceExtentInspection {
    let digest = payload
        .digest()
        .unwrap_or_else(|| DigestV1::hash_bytes(payload.bytes()));
    StreamedResourceExtentInspection::new(digest, 0, payload.len(), alignment)
}

fn assert_footprint_matches_committed_usage(
    footprint: ArtifactSetFootprint,
    usage: ArtifactBudgetUsage,
) {
    assert_eq!(footprint.outputs(), usage.outputs());
    assert_eq!(footprint.proof_images(), usage.proof_images());
    assert_eq!(footprint.publication_bytes(), usage.publication_bytes());
    assert_eq!(footprint.proof_bytes(), usage.proof_bytes());
    assert_eq!(footprint.generated_bytes(), usage.generated_bytes());
    assert_eq!(footprint.metadata_bytes(), usage.metadata_bytes());
    assert_eq!(footprint.pinned_source_bytes(), usage.pinned_source_bytes());
    assert_eq!(footprint.retained_bytes(), usage.retained_bytes());
    assert_eq!(footprint.segments(), usage.segments());
}

fn independent_reparse_source(error: ArtifactBuildError) -> ArtifactBuildError {
    assert_eq!(
        error.failure_phase(),
        ArtifactBuildFailurePhase::IndependentReparse
    );
    match error {
        ArtifactBuildError::IndependentReparse { source } => *source,
        error => panic!("expected an independent-reparse failure, got {error}"),
    }
}

#[test]
fn default_limits_expose_independent_output_proof_and_retention_ceilings() {
    let limits = ArtifactLimits::default();

    assert_eq!(limits.max_outputs(), 1_000_000);
    assert_eq!(limits.max_proof_images(), 4_000_000);
    assert_eq!(limits.max_segments(), 4_000_000);
    assert_eq!(limits.max_publication_bytes(), 8 * GIB);
    assert_eq!(limits.max_proof_bytes(), 16 * GIB);
    assert_eq!(limits.max_generated_bytes(), 2 * GIB);
    assert_eq!(limits.max_generated_chunk_bytes(), GIB);
    assert_eq!(limits.max_metadata_bytes(), 512 * MIB);
    assert_eq!(limits.max_pinned_source_bytes(), 16 * GIB);
    assert_eq!(limits.max_retained_bytes(), 20 * GIB);
    assert_eq!(limits.max_scratch_bytes(), 2 * GIB);
}

#[test]
fn source_inspection_borrows_the_batch_budget_only_for_the_call() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let observed = batch
        .inspect_with_budget(|budget| {
            budget.consume_bytes(7)?;
            Ok(budget.usage().bytes)
        })
        .unwrap();
    assert_eq!(observed, 7);

    let second = batch
        .inspect_with_budget(|budget| {
            budget.consume_entries(1)?;
            Ok(budget.usage())
        })
        .unwrap();
    assert_eq!(second.bytes, 7);
    assert_eq!(second.entries, 1);
}

#[test]
fn source_inspection_error_poisons_the_batch() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let error = batch
        .inspect_with_budget::<()>(|_| {
            Err(ArtifactBuildError::InternalInvariant {
                message: "inspection rejected",
            })
        })
        .unwrap_err();
    assert!(matches!(
        error,
        ArtifactBuildError::InternalInvariant {
            message: "inspection rejected"
        }
    ));
    assert!(matches!(
        batch.inspect_with_budget(|_| Ok(())),
        Err(ArtifactBuildError::PoisonedBatch)
    ));
}

#[test]
fn source_inspection_budget_error_poisons_the_batch() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: 3,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    assert!(matches!(
        batch.inspect_with_budget::<()>(|budget| {
            budget.consume_bytes(4)?;
            Ok(())
        }),
        Err(ArtifactBuildError::LoadBudget(_))
    ));
    assert!(matches!(
        batch.inspect_with_budget(|_| Ok(())),
        Err(ArtifactBuildError::PoisonedBatch)
    ));
}

#[test]
fn fail_stop_success_keeps_the_batch_active() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let value = batch
        .run_fail_stop(|_| Result::<_, ArtifactBuildError>::Ok(17_u8))
        .unwrap();

    assert_eq!(value, 17);
    assert!(batch.inspect_with_budget(|_| Ok(())).is_ok());
}

#[test]
fn fail_stop_domain_error_poisons_the_batch() {
    #[derive(Debug)]
    enum DomainError {
        Artifact,
        Rejected,
    }

    impl From<ArtifactBuildError> for DomainError {
        fn from(_error: ArtifactBuildError) -> Self {
            Self::Artifact
        }
    }

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    assert!(matches!(
        batch.run_fail_stop::<(), DomainError>(|_| Err(DomainError::Rejected)),
        Err(DomainError::Rejected)
    ));
    assert!(matches!(
        batch.inspect_with_budget(|_| Ok(())),
        Err(ArtifactBuildError::PoisonedBatch)
    ));
}

#[test]
fn serialized_format_is_produced_only_by_the_fixed_binary_inspector() {
    let payload = serialized_payload(20);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let slot = declaration.declare_output(name("main.assets")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let artifact = batch
        .prepare_serialized_file(payload.len(), |encoder| encoder.push_payload_full(&payload))
        .unwrap();
    batch.bind_output(slot, artifact).unwrap();
    let set = batch.finish().unwrap();

    assert!(matches!(
        set.outputs().next().unwrap().artifact().format(),
        PreparedArtifactFormat::SerializedFile(proof)
            if proof.version() == 8 && proof.byte_order() == ByteOrder::Little
    ));
}

#[test]
fn serialized_inspection_tables_are_charged_exactly_to_artifact_metadata() {
    let empty_tables = serialized_payload(20);
    let populated_tables = source_payload(
        include_bytes!("../../tests/fixtures/serialized_file_wire/v22.assets.bin").to_vec(),
        SourceKind::SerializedFile,
        21,
    );

    let mut empty_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let empty_set = prepare_serialized_set(&empty_tables, &mut empty_budget).unwrap();
    let empty_metadata = empty_budget.committed_usage().metadata_bytes();
    let empty_proof_heap = match empty_set.outputs().next().unwrap().artifact().format() {
        PreparedArtifactFormat::SerializedFile(proof) => proof.retained_heap_bytes().unwrap(),
        format => panic!("expected SerializedFile proof, found {format:?}"),
    };
    drop(empty_set);

    let mut probe_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let probe_set = prepare_serialized_set(&populated_tables, &mut probe_budget).unwrap();
    let populated_metadata = probe_budget.committed_usage().metadata_bytes();
    let proof_heap = match probe_set.outputs().next().unwrap().artifact().format() {
        PreparedArtifactFormat::SerializedFile(proof) => proof.retained_heap_bytes().unwrap(),
        format => panic!("expected SerializedFile proof, found {format:?}"),
    };
    assert!(proof_heap > empty_proof_heap);
    assert_eq!(
        populated_metadata - empty_metadata,
        proof_heap - empty_proof_heap
    );
    drop(probe_set);

    let exact_limits = ArtifactLimits::default().with_max_metadata_bytes(populated_metadata);
    let mut exact_budget = ArtifactBudget::new(exact_limits).unwrap();
    let exact_set = prepare_serialized_set(&populated_tables, &mut exact_budget).unwrap();
    assert_eq!(
        exact_budget.committed_usage().metadata_bytes(),
        populated_metadata
    );
    drop(exact_set);

    let short_limit = populated_metadata - 1;
    let short_limits = ArtifactLimits::default().with_max_metadata_bytes(short_limit);
    let mut short_budget = ArtifactBudget::new(short_limits).unwrap();
    assert!(matches!(
        prepare_serialized_set(&populated_tables, &mut short_budget),
        Err(ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
            resource: "metadata_bytes",
            requested,
            limit,
        })) if requested == populated_metadata && limit == short_limit
    ));
    assert_eq!(
        short_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(short_budget.live_scratch_bytes(), 0);
}

#[test]
fn prepared_set_resolves_only_handles_from_its_own_graph() {
    let first_payload = resource_payload(b"first", 100);
    let mut first_artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut first_inspection_budget = AssetLoadBudget::default();
    let mut first_declaration =
        ArtifactBatchDeclaration::begin(&mut first_artifact_budget, &mut first_inspection_budget)
            .unwrap();
    let first_output = first_declaration
        .declare_output(name("first.resS"))
        .unwrap();
    let mut first_batch = first_declaration.seal_output_names().unwrap();
    let first_handle = prepare_resource(&mut first_batch, &first_payload).unwrap();
    first_batch.bind_output(first_output, first_handle).unwrap();
    let first_set = first_batch.finish().unwrap();

    let second_payload = resource_payload(b"second", 101);
    let mut second_artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut second_inspection_budget = AssetLoadBudget::default();
    let mut second_declaration =
        ArtifactBatchDeclaration::begin(&mut second_artifact_budget, &mut second_inspection_budget)
            .unwrap();
    let second_output = second_declaration
        .declare_output(name("second.resS"))
        .unwrap();
    let mut second_batch = second_declaration.seal_output_names().unwrap();
    let second_handle = prepare_resource(&mut second_batch, &second_payload).unwrap();
    second_batch
        .bind_output(second_output, second_handle)
        .unwrap();
    let second_set = second_batch.finish().unwrap();

    assert!(std::ptr::eq(
        first_set.artifact(first_handle).unwrap(),
        first_set.outputs().next().unwrap().artifact()
    ));
    assert!(matches!(
        first_set.artifact(second_handle),
        Err(ArtifactBuildError::ForeignArtifactHandle { artifact: 0 })
    ));
    assert!(matches!(
        second_set.artifact(first_handle),
        Err(ArtifactBuildError::ForeignArtifactHandle { artifact: 0 })
    ));
}

#[test]
fn prepared_artifact_contiguous_ranges_borrow_only_one_segment() {
    let first = resource_payload(b"abc", 109);
    let second = resource_payload(b"def", 110);
    let extents = [
        StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b"abc"), 0, 3, 1),
        StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b"def"), 3, 3, 1),
    ];
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("segmented.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let handle = batch
        .prepare_streamed_resource_extents(extents, |encoder| {
            encoder.push_payload_full(&first)?;
            encoder.push_payload_full(&second)
        })
        .unwrap();
    batch.bind_output(output, handle).unwrap();
    let set = batch.finish().unwrap();
    let artifact = set.artifact(handle).unwrap();

    assert_eq!(artifact.contiguous_range(0..3), Some(b"abc".as_slice()));
    assert_eq!(artifact.contiguous_range(3..6), Some(b"def".as_slice()));
    assert_eq!(artifact.contiguous_range(6..6), Some(b"".as_slice()));
    assert!(artifact.contiguous_range(0..6).is_none());
    assert!(artifact.contiguous_range(2..4).is_none());
    assert!(
        artifact
            .contiguous_range(std::ops::Range { start: 5, end: 4 })
            .is_none()
    );
    assert!(artifact.contiguous_range(0..7).is_none());
}

#[test]
fn verbatim_source_leaf_retains_exact_provenance_without_generated_bytes() {
    let backing = Arc::<[u8]>::from(b"unchanged bundle member".as_slice());
    let source = SourceId::new(
        WorkspaceId::from_u128(17).unwrap(),
        SourceKind::AssetBundle,
        102,
    )
    .unwrap();
    let image = VerifiedSourceImage::verify(SourceKind::AssetBundle, Arc::clone(&backing));
    let fingerprint = image.fingerprint();
    let payload = ArtifactPayload::source_backed(source, image).unwrap();
    let initial_strong_count = Arc::strong_count(&backing);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(name("unchanged.bundle"))
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let handle = batch.prepare_verbatim_source(&payload).unwrap();
    batch.bind_output(output, handle).unwrap();
    let set = batch.finish().unwrap();
    let artifact = set.artifact(handle).unwrap();

    let PreparedArtifactFormat::VerbatimSource(proof) = artifact.format() else {
        panic!("verbatim preparation must retain a source proof");
    };
    assert_eq!(
        artifact.format().kind(),
        PreparedArtifactKind::VerbatimSource
    );
    assert_eq!(proof.source_id(), source);
    assert_eq!(proof.fingerprint(), fingerprint);
    assert_eq!(proof.length(), backing.len() as u64);
    assert_eq!(artifact.digest(), fingerprint.digest());
    assert_eq!(artifact.source_dependencies().len(), 1);
    assert_eq!(artifact.source_dependencies()[0].source(), source);
    assert_eq!(
        artifact.source_dependencies()[0].referenced_bytes(),
        backing.len() as u64
    );
    assert_eq!(artifact.footprint().generated_bytes(), 0);
    assert_eq!(artifact.build_counters().generated_chunks(), 0);
    assert_eq!(artifact.build_counters().source_ranges(), 1);
    assert_eq!(artifact.build_counters().digest_reuses(), 1);
    assert!(Arc::strong_count(&backing) > initial_strong_count);
}

#[test]
fn verbatim_source_rejects_generated_payload_and_rolls_back() {
    let generated = ArtifactPayload::from_generated_vec(Arc::new(b"generated".to_vec())).unwrap();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        declaration.declare_output(name("generated.bin")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        assert!(matches!(
            batch.prepare_verbatim_source(&generated),
            Err(ArtifactBuildError::VerbatimSourceRequiresSourcePayload)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn yaml_leaf_reparses_valid_source_bytes_and_retains_syntax_proof() {
    let payload = yaml_payload(b"root:\n  values: [1, 2, 3]\n", 103);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("scene.yaml")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let handle = batch.prepare_yaml(&payload).unwrap();
    batch.bind_output(output, handle).unwrap();
    let set = batch.finish().unwrap();
    let artifact = set.artifact(handle).unwrap();

    let PreparedArtifactFormat::Yaml(proof) = artifact.format() else {
        panic!("YAML preparation must retain a syntax proof");
    };
    assert_eq!(artifact.format().kind(), PreparedArtifactKind::Yaml);
    assert_eq!(proof.encoded_bytes(), payload.len());
    assert_eq!(proof.documents(), 1);
    assert!(proof.events() >= 10);
    assert_eq!(proof.max_depth(), 3);
    assert_eq!(artifact.source_dependencies().len(), 1);
}

#[test]
fn yaml_leaf_accepts_unity_directives_tags_and_document_anchors() {
    let payload = yaml_payload(
        b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Example\n",
        110,
    );
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(name("unity-scene.yaml"))
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let handle = batch.prepare_yaml(&payload).unwrap();
    batch.bind_output(output, handle).unwrap();
    let set = batch.finish().unwrap();

    assert!(matches!(
        set.artifact(handle).unwrap().format(),
        PreparedArtifactFormat::Yaml(proof)
            if proof.documents() == 1 && proof.encoded_bytes() == payload.len()
    ));
}

#[test]
fn yaml_writer_promotes_budgeted_generated_bytes_only_after_reparse() {
    let encoded = b"root:\n  generated: true\n";
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("generated.yaml")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let mut writer = batch.yaml_writer().unwrap();
    writer.write_all(encoded).unwrap();
    let handle = batch.prepare_yaml_writer(writer).unwrap();
    batch.bind_output(output, handle).unwrap();
    let set = batch.finish().unwrap();
    let artifact = set.artifact(handle).unwrap();

    assert!(matches!(
        artifact.format(),
        PreparedArtifactFormat::Yaml(proof)
            if proof.encoded_bytes() == encoded.len() as u64 && proof.documents() == 1
    ));
    assert!(artifact.source_dependencies().is_empty());
    assert_eq!(artifact.footprint().proof_bytes(), encoded.len() as u64);
    assert!(artifact.footprint().generated_bytes() >= encoded.len() as u64);
    assert_eq!(artifact.build_counters().generated_chunks(), 1);
    let mut actual = Vec::new();
    artifact.reader().read_to_end(&mut actual).unwrap();
    assert_eq!(actual, encoded);
}

#[test]
fn yaml_reparse_accepts_utf8_code_points_split_across_source_segments() {
    let first = yaml_payload(b"root: caf\xc3", 104);
    let second = yaml_payload(b"\xa9\n", 105);
    let declared_len = first.len() + second.len();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("unicode.yaml")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let handle = batch
        .prepare_yaml_encoded(declared_len, |encoder| {
            encoder.push_payload_full(&first)?;
            encoder.push_payload_full(&second)
        })
        .unwrap();
    batch.bind_output(output, handle).unwrap();
    let set = batch.finish().unwrap();
    let artifact = set.artifact(handle).unwrap();

    assert!(matches!(
        artifact.format(),
        PreparedArtifactFormat::Yaml(proof)
            if proof.encoded_bytes() == declared_len && proof.documents() == 1
    ));
    assert_eq!(artifact.footprint().segments(), 2);
    assert_eq!(artifact.source_dependencies().len(), 2);
}

#[test]
fn yaml_leaf_rejects_invalid_syntax_without_committing_artifact_usage() {
    let payload = yaml_payload(b"root: [1, 2\n", 106);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        declaration.declare_output(name("invalid.yaml")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let error = batch.prepare_yaml(&payload).unwrap_err();
        assert!(matches!(
            independent_reparse_source(error),
            ArtifactBuildError::InvalidYaml(_)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn yaml_leaf_rejects_invalid_segmented_utf8_with_a_typed_offset() {
    let first = yaml_payload(b"root: \xf0\x9f", 107);
    let second = yaml_payload(b"x\n", 108);
    let declared_len = first.len() + second.len();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    declaration
        .declare_output(name("invalid-utf8.yaml"))
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let error = batch
        .prepare_yaml_encoded(declared_len, |encoder| {
            encoder.push_payload_full(&first)?;
            encoder.push_payload_full(&second)
        })
        .unwrap_err();
    assert!(matches!(
        independent_reparse_source(error),
        ArtifactBuildError::InvalidYamlUtf8 { offset: 6 }
    ));
}

#[test]
fn yaml_reparse_charges_parser_work_before_parsing() {
    let payload = yaml_payload(b"root: value\n", 109);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let load_limits = unity_asset_core::AssetLoadLimits {
        max_bytes: 8 * 1024,
        ..unity_asset_core::AssetLoadLimits::default()
    };
    let mut inspection_budget = AssetLoadBudget::new(load_limits).unwrap();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    declaration.declare_output(name("budgeted.yaml")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let error = batch.prepare_yaml(&payload).unwrap_err();
    assert!(matches!(
        independent_reparse_source(error),
        ArtifactBuildError::LoadBudget(unity_asset_core::BudgetError::Exceeded {
            resource: "bytes",
            ..
        })
    ));
}

#[test]
fn batch_keeps_internal_child_proofs_and_exposes_only_output_roots() {
    let child_payload = resource_payload(b"resource", 1);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("main.assets")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let child = prepare_resource(&mut batch, &child_payload).unwrap();
    let parent = batch
        .prepare_streamed_resource_extents([resource_extent(&child_payload, 16)], |encoder| {
            encoder.append_dependency(child)
        })
        .unwrap();
    batch.bind_output(output, parent).unwrap();
    let set = batch.finish().unwrap();

    assert_eq!(set.len(), 1);
    assert_eq!(set.proof_image_count(), 2);
    let roots = set.outputs().collect::<Vec<_>>();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].name().as_str(), "main.assets");
    assert_eq!(roots[0].handle(), parent);
    assert!(matches!(
        roots[0].artifact().format(),
        PreparedArtifactFormat::StreamedResource(proof)
            if proof.length() == child_payload.len()
                && proof.extents().len() == 1
                && proof.extents()[0].alignment() == 16
    ));
    assert_eq!(set.footprint().outputs(), 1);
    assert_eq!(set.footprint().proof_images(), 2);
    assert_eq!(set.footprint().publication_bytes(), child_payload.len());
    assert_eq!(set.footprint().proof_bytes(), child_payload.len() * 2);
    assert_eq!(set.footprint().segments(), 2);
    assert_eq!(artifact_budget.usage().outputs(), 1);
    assert_eq!(artifact_budget.usage().proof_images(), 2);
    assert_eq!(
        set.footprint().metadata_bytes(),
        set.retained_metadata_bytes_for_test().unwrap()
    );
    assert_footprint_matches_committed_usage(set.footprint(), artifact_budget.committed_usage());
}

#[test]
fn zero_length_resource_extent_preserves_order_at_a_shared_offset() {
    let payload = resource_payload(b"x", 25);
    let extents = [
        StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b""), 0, 0, 16),
        StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b"x"), 0, 1, 8),
    ];
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(name("empty-first.resS"))
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let artifact = batch
        .prepare_streamed_resource_extents(extents, |encoder| encoder.push_payload_full(&payload))
        .unwrap();
    batch.bind_output(output, artifact).unwrap();
    let set = batch.finish().unwrap();
    let PreparedArtifactFormat::StreamedResource(proof) =
        set.outputs().next().unwrap().artifact().format()
    else {
        panic!("resource preparation must retain a resource proof");
    };

    assert_eq!(proof.length(), 1);
    assert_eq!(proof.payload_bytes(), 1);
    assert_eq!(proof.padding_bytes(), 0);
    assert_eq!(proof.extents().len(), 2);
    assert_eq!(proof.extents()[0].offset(), 0);
    assert_eq!(proof.extents()[0].length(), 0);
    assert_eq!(
        proof.extents()[0].payload_digest(),
        DigestV1::hash_bytes(b"")
    );
    assert_eq!(proof.extents()[1].offset(), 0);
    assert_eq!(proof.extents()[1].length(), 1);
    assert_eq!(
        proof.extents()[1].payload_digest(),
        DigestV1::hash_bytes(b"x")
    );
}

#[test]
fn streamed_resource_proof_accepts_only_zero_alignment_padding() {
    let payload = resource_payload(b"a\0\0\0bc", 26);
    let extents = [
        StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b"a"), 0, 1, 1),
        StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b"bc"), 4, 2, 4),
    ];
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("padded.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let artifact = batch
        .prepare_streamed_resource_extents(extents, |encoder| encoder.push_payload_full(&payload))
        .unwrap();
    batch.bind_output(output, artifact).unwrap();
    let set = batch.finish().unwrap();
    let PreparedArtifactFormat::StreamedResource(proof) =
        set.outputs().next().unwrap().artifact().format()
    else {
        panic!("resource preparation must retain a resource proof");
    };

    assert_eq!(proof.length(), 6);
    assert_eq!(proof.payload_bytes(), 3);
    assert_eq!(proof.padding_bytes(), 3);
    assert_eq!(proof.extents()[0].padding_before(), 0);
    assert_eq!(proof.extents()[1].padding_before(), 3);
    assert_eq!(proof.extents()[1].alignment(), 4);
}

#[test]
fn nonzero_resource_padding_poisoned_batch_does_not_commit() {
    let payload = resource_payload(b"a\0\x01\0bc", 27);
    let extents = [
        StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b"a"), 0, 1, 1),
        StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b"bc"), 4, 2, 4),
    ];
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();

        let error = batch
            .prepare_streamed_resource_extents(extents, |encoder| {
                encoder.push_payload_full(&payload)
            })
            .unwrap_err();
        assert!(matches!(
            independent_reparse_source(error),
            ArtifactBuildError::NonZeroStreamedResourcePadding { ordinal: 1 }
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn resource_payload_digest_mismatch_poisoned_batch_does_not_commit() {
    let payload = resource_payload(b"actual", 28);
    let extent = StreamedResourceExtentInspection::new(
        DigestV1::hash_bytes(b"expected"),
        0,
        payload.len(),
        1,
    );
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();

        let error = batch
            .prepare_streamed_resource_extents([extent], |encoder| {
                encoder.push_payload_full(&payload)
            })
            .unwrap_err();
        assert!(matches!(
            independent_reparse_source(error),
            ArtifactBuildError::StreamedResourcePayloadDigestMismatch { ordinal: 0, .. }
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn streamed_resource_layout_builder_rejects_foreign_batches_atomically() {
    let extent = StreamedResourceExtentInspection::new(DigestV1::hash_bytes(b""), 0, 0, 1);
    let mut first_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut first_inspection = AssetLoadBudget::default();
    let first_declaration =
        ArtifactBatchDeclaration::begin(&mut first_budget, &mut first_inspection).unwrap();
    let first_batch = first_declaration.seal_output_names().unwrap();
    let mut builder = first_batch.streamed_resource_layout_builder().unwrap();
    builder.push(extent).unwrap();

    let mut second_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut second_inspection = AssetLoadBudget::default();
    {
        let second_declaration =
            ArtifactBatchDeclaration::begin(&mut second_budget, &mut second_inspection).unwrap();
        let mut second_batch = second_declaration.seal_output_names().unwrap();
        assert!(matches!(
            second_batch.prepare_streamed_resource(builder, |_| Ok(())),
            Err(ArtifactBuildError::ForeignStreamedResourceLayout)
        ));
        assert!(matches!(
            second_batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    drop(first_batch);
    assert_eq!(
        first_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(first_budget.live_scratch_bytes(), 0);
    assert_eq!(
        second_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(second_budget.live_scratch_bytes(), 0);
}

#[test]
fn streamed_resource_layout_builder_failure_is_sticky_and_releases_scratch() {
    let limits = ArtifactLimits::default().with_max_scratch_bytes(1);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let mut builder = batch.streamed_resource_layout_builder().unwrap();
        assert!(matches!(
            builder.push(StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(b""),
                0,
                0,
                1,
            )),
            Err(ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                resource: "scratch_bytes",
                ..
            }))
        ));
        assert!(matches!(
            builder.push(StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(b""),
                0,
                0,
                1,
            )),
            Err(ArtifactBuildError::PoisonedStreamedResourceLayout)
        ));
        assert!(matches!(
            batch.prepare_streamed_resource(builder, |_| Ok(())),
            Err(ArtifactBuildError::PoisonedStreamedResourceLayout)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn resource_inspection_extent_allocation_is_retained_and_charged_once() {
    let payload = resource_payload(b"proof", 38);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("proof.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let mut builder = batch.streamed_resource_layout_builder().unwrap();
    builder.push(resource_extent(&payload, 1)).unwrap();
    let artifact = batch
        .prepare_streamed_resource(builder, |encoder| encoder.push_payload_full(&payload))
        .unwrap();
    batch.bind_output(output, artifact).unwrap();
    let set = batch.finish().unwrap();
    let root = set.outputs().next().unwrap().artifact();
    let expected_extent_allocation =
        vec_allocation_bytes::<StreamedResourceExtentInspection>(8).unwrap();

    assert_eq!(
        root.footprint().inspection_bytes(),
        expected_extent_allocation
    );
    assert_eq!(
        set.footprint().metadata_bytes(),
        set.retained_metadata_bytes_for_test().unwrap()
    );
    assert_footprint_matches_committed_usage(set.footprint(), artifact_budget.committed_usage());
}

#[test]
fn output_iteration_preserves_declaration_order_not_prepare_order() {
    let first_payload = resource_payload(b"first", 3);
    let second_payload = resource_payload(b"second", 4);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let first_slot = declaration.declare_output(name("a.resS")).unwrap();
    let second_slot = declaration.declare_output(name("b.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let second = prepare_resource(&mut batch, &second_payload).unwrap();
    let first = prepare_resource(&mut batch, &first_payload).unwrap();
    batch.bind_output(first_slot, first).unwrap();
    batch.bind_output(second_slot, second).unwrap();
    let set = batch.finish().unwrap();

    let names = set
        .outputs()
        .map(|output| output.name().as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["a.resS", "b.resS"]);
    assert_eq!(set.output(first_slot).unwrap().name().as_str(), "a.resS");
    assert_eq!(set.output(second_slot).unwrap().name().as_str(), "b.resS");
}

#[test]
fn generated_chunks_are_promoted_once_and_counted_as_proof_then_publication() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let slot = declaration.declare_output(name("generated.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let mut writer = batch.generated_chunk_writer().unwrap();
    writer.extend_from_slice(b"generated").unwrap();
    let payload = batch.finish_generated_chunk(writer).unwrap();

    let artifact = prepare_resource(&mut batch, &payload).unwrap();
    batch.bind_output(slot, artifact).unwrap();
    let set = batch.finish().unwrap();
    let output = set.outputs().next().unwrap();
    let mut bytes = Vec::new();
    output.artifact().reader().read_to_end(&mut bytes).unwrap();

    assert_eq!(bytes, b"generated");
    assert_eq!(set.footprint().proof_bytes(), 9);
    assert_eq!(set.footprint().publication_bytes(), 9);
    assert!(set.footprint().generated_bytes() >= 9);
    assert_eq!(output.artifact().build_counters().digest_passes(), 1);
    assert_eq!(output.artifact().build_counters().digest_reuses(), 0);
}

#[test]
fn name_collision_fails_at_seal_and_rolls_back_artifact_usage() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    declaration
        .declare_output(name("Assets/Main.assets"))
        .unwrap();
    declaration
        .declare_output(name("assets/main.ASSETS"))
        .unwrap();

    assert!(matches!(
        declaration.seal_output_names(),
        Err(ArtifactBuildError::Name(
            ArtifactNameError::PortabilityCollision { .. }
        ))
    ));
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn failed_output_declaration_growth_is_fail_stop_and_cannot_commit() {
    let limits = ArtifactLimits::default().with_max_scratch_bytes(1);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();

    assert!(matches!(
        declaration.declare_output(name("cannot-grow.resS")),
        Err(ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
            resource: "scratch_bytes",
            ..
        }))
    ));
    assert!(matches!(
        declaration.declare_output(name("retry.resS")),
        Err(ArtifactBuildError::PoisonedDeclaration)
    ));
    assert!(matches!(
        declaration.seal_output_names(),
        Err(ArtifactBuildError::PoisonedDeclaration)
    ));
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn invalid_serialized_image_consumes_inspection_budget_but_not_artifact_budget() {
    let invalid = source_payload(
        b"not a serialized file".to_vec(),
        SourceKind::SerializedFile,
        5,
    );
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        declaration.declare_output(name("bad.assets")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();

        let error = batch
            .prepare_serialized_file(invalid.len(), |encoder| encoder.push_payload_full(&invalid))
            .unwrap_err();
        assert_eq!(
            error.failure_phase(),
            ArtifactBuildFailurePhase::IndependentReparse
        );
        assert!(matches!(
            error,
            ArtifactBuildError::IndependentReparse { source }
                if matches!(*source, ArtifactBuildError::Binary(_))
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }

    assert_ne!(inspection_budget.usage(), AssetLoadUsage::default());
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn encoding_failure_is_not_retagged_as_an_independent_reparse() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    declaration.declare_output(name("bad.assets")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();

    let error = batch
        .prepare_serialized_file(0, |_| {
            Err(ArtifactBuildError::InternalInvariant {
                message: "injected encoding failure",
            })
        })
        .unwrap_err();

    assert_eq!(error.failure_phase(), ArtifactBuildFailurePhase::Encoding);
    assert!(matches!(
        error,
        ArtifactBuildError::InternalInvariant {
            message: "injected encoding failure"
        }
    ));
}

#[test]
fn unbound_output_and_unreachable_proof_graphs_do_not_commit() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        declaration.declare_output(name("missing.resS")).unwrap();
        let batch = declaration.seal_output_names().unwrap();
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::UnboundOutput { output: 0 })
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );

    let orphan = resource_payload(b"orphan", 6);
    let root = resource_payload(b"root", 7);
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let slot = declaration.declare_output(name("root.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        prepare_resource(&mut batch, &orphan).unwrap();
        let root = prepare_resource(&mut batch, &root).unwrap();
        batch.bind_output(slot, root).unwrap();
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::UnreachableArtifact { artifact: 0 })
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn repeated_dependency_consumption_deduplicates_the_reachable_edge() {
    let payload = resource_payload(b"child", 8);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let slot = declaration.declare_output(name("parent.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let child = prepare_resource(&mut batch, &payload).unwrap();
    let payload_digest = payload.digest().unwrap();
    let extents = [
        StreamedResourceExtentInspection::new(payload_digest, 0, payload.len(), 1),
        StreamedResourceExtentInspection::new(payload_digest, payload.len(), payload.len(), 1),
    ];
    let parent = batch
        .prepare_streamed_resource_extents(extents, |encoder| {
            encoder.append_dependency(child)?;
            encoder.append_dependency(child)
        })
        .unwrap();
    batch.bind_output(slot, parent).unwrap();
    let set = batch.finish().unwrap();
    let root = set.outputs().next().unwrap().artifact();
    let backing_bytes = payload.backing().allocation_bytes().unwrap();

    assert_eq!(set.proof_image_count(), 2);
    assert_eq!(set.footprint().proof_bytes(), payload.len() * 3);
    assert_eq!(set.footprint().referenced_source_bytes(), payload.len() * 3);
    assert_eq!(set.footprint().pinned_source_bytes(), backing_bytes);
    assert_eq!(root.source_dependencies().len(), 1);
    assert_eq!(
        root.source_dependencies()[0].referenced_bytes(),
        payload.len() * 2
    );
    assert_eq!(
        root.footprint().referenced_source_bytes(),
        payload.len() * 2
    );
    assert_eq!(root.footprint().pinned_source_bytes(), backing_bytes);
    assert_eq!(root.build_counters().source_ranges(), 0);
    assert_eq!(set.build_counters().source_ranges(), 1);
}

#[test]
fn dependency_subrange_propagates_only_intersecting_source_usage() {
    let first = resource_payload(b"first", 31);
    let second = resource_payload(b"second", 32);
    let first_source = match first.provenance() {
        ArtifactPayloadProvenance::Source { source_id, .. } => source_id,
        ArtifactPayloadProvenance::Generated => panic!("fixture must be source backed"),
    };
    let child_extents = [
        StreamedResourceExtentInspection::new(first.digest().unwrap(), 0, first.len(), 1),
        StreamedResourceExtentInspection::new(
            second.digest().unwrap(),
            first.len(),
            second.len(),
            1,
        ),
    ];
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("first-only.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let child = batch
        .prepare_streamed_resource_extents(child_extents, |encoder| {
            encoder.push_payload_full(&first)?;
            encoder.push_payload_full(&second)
        })
        .unwrap();
    let parent = batch
        .prepare_streamed_resource_extents([resource_extent(&first, 1)], |encoder| {
            encoder.append_dependency_range(child, 0..first.len())
        })
        .unwrap();
    batch.bind_output(output, parent).unwrap();
    let set = batch.finish().unwrap();
    let root = set.outputs().next().unwrap().artifact();

    assert_eq!(root.source_dependencies().len(), 1);
    assert_eq!(root.source_dependencies()[0].source(), first_source);
    assert_eq!(
        root.source_dependencies()[0].referenced_bytes(),
        first.len()
    );
    assert_eq!(root.footprint().referenced_source_bytes(), first.len());
    assert_eq!(
        root.footprint().pinned_source_bytes(),
        first.backing().allocation_bytes().unwrap()
    );
    assert_eq!(root.build_counters().source_ranges(), 0);
    assert_eq!(set.build_counters().source_ranges(), 2);
}

#[test]
fn dependency_tail_subrange_selects_only_the_intersecting_segment_usage() {
    const COUNT: usize = 128;

    let payloads = (0..COUNT)
        .map(|ordinal| resource_payload(b"x", 1_000 + ordinal as u128))
        .collect::<Vec<_>>();
    let tail_source = match payloads.last().unwrap().provenance() {
        ArtifactPayloadProvenance::Source { source_id, .. } => source_id,
        ArtifactPayloadProvenance::Generated => panic!("fixture must be source backed"),
    };
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("tail.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let mut child_layout = batch.streamed_resource_layout_builder().unwrap();
    for (ordinal, payload) in payloads.iter().enumerate() {
        child_layout
            .push(StreamedResourceExtentInspection::new(
                payload.digest().unwrap(),
                ordinal as u64,
                1,
                1,
            ))
            .unwrap();
    }
    let child = batch
        .prepare_streamed_resource(child_layout, |encoder| {
            for payload in &payloads {
                encoder.push_payload_full(payload)?;
            }
            Ok(())
        })
        .unwrap();
    let parent = batch
        .prepare_streamed_resource_extents(
            [StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(b"x"),
                0,
                1,
                1,
            )],
            |encoder| encoder.append_dependency_range(child, (COUNT as u64 - 1)..COUNT as u64),
        )
        .unwrap();
    batch.bind_output(output, parent).unwrap();
    let set = batch.finish().unwrap();
    let root = set.outputs().next().unwrap().artifact();

    assert_eq!(root.footprint().segments(), 1);
    assert_eq!(root.source_dependencies().len(), 1);
    assert_eq!(root.source_dependencies()[0].source(), tail_source);
    assert_eq!(root.source_dependencies()[0].referenced_bytes(), 1);
    assert_eq!(root.build_counters().source_ranges(), 0);
    assert_eq!(set.build_counters().source_ranges(), COUNT as u64);
}

#[test]
fn merely_requesting_a_dependency_reader_does_not_create_a_proof_edge() {
    let child_payload = resource_payload(b"child", 21);
    let parent_payload = resource_payload(b"parent", 22);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let slot = declaration.declare_output(name("parent.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let child = prepare_resource(&mut batch, &child_payload).unwrap();
    let derived = batch
        .derive_generated_chunk(|encoder| {
            let mut writer = encoder.generated_chunk_writer()?;
            {
                let _unused = encoder.dependency_reader(child)?;
            }
            writer.write_all(parent_payload.bytes())?;
            encoder.finish_generated_chunk(writer)
        })
        .unwrap();
    let parent = batch
        .prepare_streamed_resource_extents([resource_extent(&parent_payload, 16)], |encoder| {
            encoder.push_derived_generated_chunk(derived)
        })
        .unwrap();
    batch.bind_output(slot, parent).unwrap();

    assert!(matches!(
        batch.finish(),
        Err(ArtifactBuildError::UnreachableArtifact { artifact: 0 })
    ));
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn explicit_empty_dependency_is_a_reachable_semantic_edge() {
    let empty = resource_payload(b"", 33);
    let (empty_source, empty_fingerprint) = match empty.provenance() {
        ArtifactPayloadProvenance::Source {
            source_id,
            fingerprint,
        } => (source_id, fingerprint),
        ArtifactPayloadProvenance::Generated => panic!("fixture must be source backed"),
    };
    let parent_payload = resource_payload(b"parent", 34);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("parent.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let child = prepare_resource(&mut batch, &empty).unwrap();
    let parent = batch
        .prepare_streamed_resource_extents([resource_extent(&parent_payload, 1)], |encoder| {
            encoder.append_dependency(child)?;
            encoder.push_payload_full(&parent_payload)
        })
        .unwrap();
    batch.bind_output(output, parent).unwrap();

    let set = batch.finish().unwrap();
    assert_eq!(set.proof_image_count(), 2);
    assert_eq!(
        set.outputs().next().unwrap().artifact().len(),
        parent_payload.len()
    );
    let empty_dependency = set
        .source_dependencies()
        .iter()
        .find(|dependency| dependency.source() == empty_source)
        .unwrap();
    assert_eq!(empty_dependency.fingerprint(), empty_fingerprint);
    assert_eq!(empty_dependency.referenced_bytes(), 0);
}

#[test]
fn empty_source_output_retains_identity_without_pinning_an_unused_backing() {
    let empty = resource_payload(b"", 40);
    let (source, fingerprint) = match empty.provenance() {
        ArtifactPayloadProvenance::Source {
            source_id,
            fingerprint,
        } => (source_id, fingerprint),
        ArtifactPayloadProvenance::Generated => panic!("fixture must be source backed"),
    };
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("empty.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let artifact = prepare_resource(&mut batch, &empty).unwrap();
    batch.bind_output(output, artifact).unwrap();
    let set = batch.finish().unwrap();
    let root = set.outputs().next().unwrap().artifact();

    assert_eq!(root.footprint().segments(), 0);
    assert_eq!(root.footprint().pinned_source_bytes(), 0);
    assert_eq!(root.source_dependencies().len(), 1);
    assert_eq!(set.source_dependencies().len(), 1);
    assert_eq!(set.source_dependencies()[0].source(), source);
    assert_eq!(set.source_dependencies()[0].fingerprint(), fingerprint);
    assert_eq!(set.source_dependencies()[0].referenced_bytes(), 0);
    assert_eq!(set.footprint().referenced_source_bytes(), 0);
    assert_eq!(set.footprint().pinned_source_bytes(), 0);
}

#[test]
fn constant_empty_image_has_no_source_dependency() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(name("constant-empty.resS"))
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let artifact = batch
        .prepare_streamed_resource_extents(
            [StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(b""),
                0,
                0,
                1,
            )],
            |_| Ok(()),
        )
        .unwrap();
    batch.bind_output(output, artifact).unwrap();
    let set = batch.finish().unwrap();

    assert!(set.source_dependencies().is_empty());
    assert!(
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .source_dependencies()
            .is_empty()
    );
    assert_eq!(set.footprint().pinned_source_bytes(), 0);
}

#[test]
fn duplicate_empty_source_proofs_merge_without_pinning_backings() {
    let empty = resource_payload(b"", 41);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let first_output = declaration.declare_output(name("first.resS")).unwrap();
    let second_output = declaration.declare_output(name("second.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let first = prepare_resource(&mut batch, &empty).unwrap();
    let second = prepare_resource(&mut batch, &empty).unwrap();
    batch.bind_output(first_output, first).unwrap();
    batch.bind_output(second_output, second).unwrap();
    let set = batch.finish().unwrap();

    assert_eq!(set.source_dependencies().len(), 1);
    assert_eq!(set.source_dependencies()[0].referenced_bytes(), 0);
    assert_eq!(set.footprint().pinned_source_bytes(), 0);
}

#[test]
fn empty_source_fingerprint_conflict_is_typed_and_atomic() {
    let empty = resource_payload(b"", 42);
    let conflicting = resource_payload(b"nonempty", 42);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        prepare_resource(&mut batch, &empty).unwrap();

        assert!(matches!(
            prepare_resource(&mut batch, &conflicting),
            Err(ArtifactBuildError::Budget(
                ArtifactBudgetError::ConflictingSourceFingerprint { .. }
            ))
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn swallowed_encoder_error_still_poisoned_the_batch() {
    let payload = resource_payload(b"root", 35);
    let extent = resource_extent(&payload, 1);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();

        assert!(matches!(
            batch.prepare_streamed_resource_extents([extent], |encoder| {
                assert!(matches!(
                    encoder.push_payload_range(&payload, 0..payload.bytes().len() + 1),
                    Err(ArtifactBuildError::InvalidBackingRange { .. })
                ));
                Ok(())
            }),
            Err(ArtifactBuildError::PoisonedEncoder)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn consuming_dependency_reader_bytes_creates_the_reachable_edge() {
    let child_payload = resource_payload(b"child", 23);
    let parent_payload = resource_payload(b"parent", 24);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let slot = declaration.declare_output(name("parent.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let child = prepare_resource(&mut batch, &child_payload).unwrap();
    let derived = batch
        .derive_generated_chunk(|encoder| {
            let mut writer = encoder.generated_chunk_writer()?;
            {
                let mut dependency = encoder.dependency_reader(child)?;
                let mut consumed = [0_u8; 1];
                dependency.read_exact(&mut consumed)?;
            }
            writer.write_all(parent_payload.bytes())?;
            encoder.finish_generated_chunk(writer)
        })
        .unwrap();
    let parent = batch
        .prepare_streamed_resource_extents([resource_extent(&parent_payload, 16)], |encoder| {
            encoder.push_derived_generated_chunk(derived)
        })
        .unwrap();
    batch.bind_output(slot, parent).unwrap();
    let set = batch.finish().unwrap();

    assert_eq!(set.proof_image_count(), 2);
}

#[test]
fn encoder_generated_sink_streams_a_child_without_source_materialization() {
    let child_payload = resource_payload(b"child", 36);
    let (child_source, child_fingerprint) = match child_payload.provenance() {
        ArtifactPayloadProvenance::Source {
            source_id,
            fingerprint,
        } => (source_id, fingerprint),
        ArtifactPayloadProvenance::Generated => panic!("fixture must be source backed"),
    };
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("generated.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let child = prepare_resource(&mut batch, &child_payload).unwrap();
    let derived = batch
        .derive_generated_chunk(|encoder| {
            let mut writer = encoder.generated_chunk_writer()?;
            {
                let mut reader = encoder.dependency_reader(child)?;
                std::io::copy(&mut reader, &mut writer)?;
            }
            encoder.finish_generated_chunk(writer)
        })
        .unwrap();
    let parent = batch
        .prepare_streamed_resource_extents(
            [StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(b"child"),
                0,
                5,
                1,
            )],
            |encoder| encoder.push_derived_generated_chunk(derived),
        )
        .unwrap();
    batch.bind_output(output, parent).unwrap();
    let set = batch.finish().unwrap();
    let root = set.outputs().next().unwrap().artifact();

    assert!(root.source_dependencies().is_empty());
    assert_eq!(set.source_dependencies().len(), 1);
    assert_eq!(set.source_dependencies()[0].source(), child_source);
    assert_eq!(
        set.source_dependencies()[0].fingerprint(),
        child_fingerprint
    );
    assert_eq!(set.source_dependencies()[0].referenced_bytes(), 5);
    assert_eq!(set.footprint().referenced_source_bytes(), 5);
}

#[test]
fn derived_generated_chunk_carries_the_dependencies_it_actually_reads() {
    let child_payload = resource_payload(b"child", 43);
    let (child_source, child_fingerprint) = match child_payload.provenance() {
        ArtifactPayloadProvenance::Source {
            source_id,
            fingerprint,
        } => (source_id, fingerprint),
        ArtifactPayloadProvenance::Generated => panic!("fixture must be source backed"),
    };
    let expected = b"encoded:child";
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("derived.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let child = prepare_resource(&mut batch, &child_payload).unwrap();
    let derived = batch
        .derive_generated_chunk(|encoder| {
            let mut writer = encoder.generated_chunk_writer()?;
            writer.write_all(b"encoded:")?;
            {
                let mut reader = encoder.dependency_reader(child)?;
                std::io::copy(&mut reader, &mut writer)?;
            }
            encoder.finish_generated_chunk(writer)
        })
        .unwrap();
    assert_eq!(derived.len(), expected.len() as u64);
    let root = batch
        .prepare_streamed_resource_extents(
            [StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(expected),
                0,
                expected.len() as u64,
                1,
            )],
            move |encoder| encoder.push_derived_generated_chunk(derived),
        )
        .unwrap();
    batch.bind_output(output, root).unwrap();
    let set = batch.finish().unwrap();
    let root = set.outputs().next().unwrap().artifact();

    assert_eq!(set.proof_image_count(), 2);
    assert!(root.source_dependencies().is_empty());
    assert_eq!(set.source_dependencies().len(), 1);
    assert_eq!(set.source_dependencies()[0].source(), child_source);
    assert_eq!(
        set.source_dependencies()[0].fingerprint(),
        child_fingerprint
    );
    assert_eq!(set.source_dependencies()[0].referenced_bytes(), 5);
    assert_eq!(set.footprint().referenced_source_bytes(), 5);
}

#[test]
fn derived_generated_chunk_can_explicitly_depend_on_an_empty_child() {
    let empty = resource_payload(b"", 44);
    let (source, fingerprint) = match empty.provenance() {
        ArtifactPayloadProvenance::Source {
            source_id,
            fingerprint,
        } => (source_id, fingerprint),
        ArtifactPayloadProvenance::Generated => panic!("fixture must be source backed"),
    };
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(name("empty-child.resS"))
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let child = prepare_resource(&mut batch, &empty).unwrap();
    let derived = batch
        .derive_generated_chunk(|encoder| {
            encoder.record_empty_dependency(child)?;
            let mut writer = encoder.generated_chunk_writer()?;
            writer.write_all(b"marker")?;
            encoder.finish_generated_chunk(writer)
        })
        .unwrap();
    let root = batch
        .prepare_streamed_resource_extents(
            [StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(b"marker"),
                0,
                6,
                1,
            )],
            move |encoder| encoder.push_derived_generated_chunk(derived),
        )
        .unwrap();
    batch.bind_output(output, root).unwrap();
    let set = batch.finish().unwrap();

    assert_eq!(set.proof_image_count(), 2);
    assert_eq!(set.source_dependencies().len(), 1);
    assert_eq!(set.source_dependencies()[0].source(), source);
    assert_eq!(set.source_dependencies()[0].fingerprint(), fingerprint);
    assert_eq!(set.source_dependencies()[0].referenced_bytes(), 0);
}

#[test]
fn derived_generated_chunk_writer_failure_is_fail_stop() {
    let limits = ArtifactLimits::default().with_max_generated_chunk_bytes(3);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();

        assert!(matches!(
            batch.derive_generated_chunk(|encoder| {
                let mut writer = encoder.generated_chunk_writer()?;
                assert!(writer.write_all(b"long").is_err());
                Ok(())
            }),
            Err(ArtifactBuildError::PoisonedDerivedGeneratedChunk)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn derived_generated_chunk_requires_exactly_one_finished_payload() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();

        assert!(matches!(
            batch.derive_generated_chunk(|_| Ok(())),
            Err(ArtifactBuildError::UnfinishedDerivedGeneratedChunk)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn discarded_derived_generated_chunk_cannot_commit_and_releases_scratch() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let derived = batch
            .derive_generated_chunk(|encoder| {
                let mut writer = encoder.generated_chunk_writer()?;
                writer.write_all(b"orphan")?;
                encoder.finish_generated_chunk(writer)
            })
            .unwrap();
        assert_eq!(derived.len(), 6);
        drop(derived);

        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::Budget(
                ArtifactBudgetError::UnretainedGeneratedBackings { count: 1 }
            ))
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn derived_generated_chunk_is_bound_to_its_originating_batch() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let derived = {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let derived = batch
            .derive_generated_chunk(|encoder| {
                let mut writer = encoder.generated_chunk_writer()?;
                writer.write_all(b"foreign")?;
                encoder.finish_generated_chunk(writer)
            })
            .unwrap();
        drop(batch);
        derived
    };

    let mut second_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut second_inspection = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut second_budget, &mut second_inspection).unwrap();
        let output = declaration.declare_output(name("foreign.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let result = batch.prepare_streamed_resource_extents(
            [StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(b"foreign"),
                0,
                7,
                1,
            )],
            move |encoder| encoder.push_derived_generated_chunk(derived),
        );
        assert!(matches!(
            result,
            Err(ArtifactBuildError::ForeignDerivedGeneratedChunk)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
        let _ = output;
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
    assert_eq!(
        second_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(second_budget.live_scratch_bytes(), 0);
}

#[test]
fn set_source_dependencies_merge_same_source_across_multiple_children() {
    let payload = resource_payload(b"same", 37);
    let source = match payload.provenance() {
        ArtifactPayloadProvenance::Source { source_id, .. } => source_id,
        ArtifactPayloadProvenance::Generated => panic!("fixture must be source backed"),
    };
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration.declare_output(name("merged.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let first = prepare_resource(&mut batch, &payload).unwrap();
    let second = prepare_resource(&mut batch, &payload).unwrap();
    let derived = batch
        .derive_generated_chunk(|encoder| {
            let mut writer = encoder.generated_chunk_writer()?;
            for child in [first, second] {
                let mut reader = encoder.dependency_reader(child)?;
                std::io::copy(&mut reader, &mut writer)?;
            }
            encoder.finish_generated_chunk(writer)
        })
        .unwrap();
    let parent = batch
        .prepare_streamed_resource_extents(
            [StreamedResourceExtentInspection::new(
                DigestV1::hash_bytes(b"samesame"),
                0,
                8,
                1,
            )],
            |encoder| encoder.push_derived_generated_chunk(derived),
        )
        .unwrap();
    batch.bind_output(output, parent).unwrap();
    let set = batch.finish().unwrap();

    assert_eq!(set.source_dependencies().len(), 1);
    assert_eq!(set.source_dependencies()[0].source(), source);
    assert_eq!(set.source_dependencies()[0].referenced_bytes(), 8);
    assert_eq!(set.footprint().referenced_source_bytes(), 8);
}

#[test]
fn set_source_dependency_aggregation_budget_failure_rolls_back_atomically() {
    let payload = resource_payload(b"source", 39);
    let metadata_before_finish = {
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut inspection = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut budget, &mut inspection).unwrap();
        let output = declaration.declare_output(name("source.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let artifact = prepare_resource(&mut batch, &payload).unwrap();
        batch.bind_output(output, artifact).unwrap();
        batch.pending_usage().metadata_bytes()
    };
    let aggregate_allocation = vec_allocation_bytes::<ArtifactSourceDependency>(8).unwrap();
    let metadata_limit = metadata_before_finish
        .checked_add(aggregate_allocation)
        .unwrap()
        - 1;
    let limits = ArtifactLimits::default().with_max_metadata_bytes(metadata_limit);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let output = declaration.declare_output(name("source.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let artifact = prepare_resource(&mut batch, &payload).unwrap();
        batch.bind_output(output, artifact).unwrap();
        assert_eq!(
            batch.pending_usage().metadata_bytes(),
            metadata_before_finish
        );

        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                resource: "metadata_bytes",
                requested,
                limit,
            })) if requested == metadata_before_finish + aggregate_allocation
                && limit == metadata_limit
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn generated_sink_write_failure_is_sticky_for_the_encoder_and_batch() {
    let limits = ArtifactLimits::default().with_max_generated_chunk_bytes(3);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();

        assert!(matches!(
            batch.prepare_streamed_resource_extents(
                [StreamedResourceExtentInspection::new(
                    DigestV1::hash_bytes(b"long"),
                    0,
                    4,
                    1,
                )],
                |encoder| {
                    let mut writer = encoder.generated_chunk_writer()?;
                    assert!(writer.write_all(b"long").is_err());
                    drop(writer);
                    Ok(())
                }
            ),
            Err(ArtifactBuildError::PoisonedEncoder)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn generated_sink_finish_failure_is_sticky_for_the_encoder_and_batch() {
    let limits = ArtifactLimits::default().with_max_generated_bytes(1);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();

        assert!(matches!(
            batch.prepare_streamed_resource_extents(
                [StreamedResourceExtentInspection::new(
                    DigestV1::hash_bytes(b"long"),
                    0,
                    4,
                    1,
                )],
                |encoder| {
                    let mut writer = encoder.generated_chunk_writer()?;
                    writer.write_all(b"long")?;
                    assert!(encoder.finish_generated_chunk(writer).is_err());
                    Ok(())
                }
            ),
            Err(ArtifactBuildError::PoisonedEncoder)
        ));
        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn output_and_artifact_double_binding_have_distinct_errors() {
    let payload = resource_payload(b"root", 10);
    let mut first_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut first_inspection = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut first_budget, &mut first_inspection).unwrap();
        let slot = declaration.declare_output(name("one.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let artifact = prepare_resource(&mut batch, &payload).unwrap();
        batch.bind_output(slot, artifact).unwrap();
        assert!(matches!(
            batch.bind_output(slot, artifact),
            Err(ArtifactBuildError::OutputAlreadyBound { output: 0 })
        ));
    }
    assert_eq!(
        first_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );

    let mut second_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut second_inspection = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut second_budget, &mut second_inspection).unwrap();
        let first = declaration.declare_output(name("one.resS")).unwrap();
        let second = declaration.declare_output(name("two.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let artifact = prepare_resource(&mut batch, &payload).unwrap();
        batch.bind_output(first, artifact).unwrap();
        assert!(matches!(
            batch.bind_output(second, artifact),
            Err(ArtifactBuildError::ArtifactAlreadyBound { artifact: 0 })
        ));
    }
    assert_eq!(
        second_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn foreign_slots_and_handles_are_rejected_without_disclosing_tokens() {
    let payload = resource_payload(b"root", 11);
    let mut first_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut first_inspection = AssetLoadBudget::default();
    let mut first_declaration =
        ArtifactBatchDeclaration::begin(&mut first_budget, &mut first_inspection).unwrap();
    let first_slot = first_declaration
        .declare_output(name("first.resS"))
        .unwrap();
    let mut first_batch = first_declaration.seal_output_names().unwrap();
    let first_artifact = prepare_resource(&mut first_batch, &payload).unwrap();

    let mut second_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut second_inspection = AssetLoadBudget::default();
    let mut second_declaration =
        ArtifactBatchDeclaration::begin(&mut second_budget, &mut second_inspection).unwrap();
    let second_slot = second_declaration
        .declare_output(name("second.resS"))
        .unwrap();
    let mut second_batch = second_declaration.seal_output_names().unwrap();
    let _second_artifact = prepare_resource(&mut second_batch, &payload).unwrap();

    assert!(!format!("{first_slot:?}").contains("token"));
    assert!(!format!("{first_artifact:?}").contains("token"));
    assert!(matches!(
        first_batch.bind_output(second_slot, first_artifact),
        Err(ArtifactBuildError::ForeignOutputSlot { output: 0 })
    ));
    assert!(matches!(
        second_batch.bind_output(second_slot, first_artifact),
        Err(ArtifactBuildError::ForeignArtifactHandle { artifact: 0 })
    ));
}

#[test]
fn publication_limit_is_charged_only_when_a_root_is_bound() {
    let payload = resource_payload(b"four", 12);
    let limits = ArtifactLimits::default().with_max_publication_bytes(3);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let slot = declaration.declare_output(name("four.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let artifact = prepare_resource(&mut batch, &payload).unwrap();
        assert!(matches!(
            batch.bind_output(slot, artifact),
            Err(ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                resource: "publication_bytes",
                requested: 4,
                limit: 3,
            }))
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn retained_limit_covers_names_records_and_backings_as_one_ceiling() {
    let limits = ArtifactLimits::default().with_max_retained_bytes(1);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    assert!(matches!(
        declaration.declare_output(name("too-large.resS")),
        Err(ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
            resource: "retained_bytes",
            ..
        }))
    ));
    assert!(matches!(
        declaration.seal_output_names(),
        Err(ArtifactBuildError::PoisonedDeclaration)
    ));
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
}

#[test]
fn dropped_generated_payload_cannot_commit_ghost_retention() {
    let root = resource_payload(b"root", 29);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let output = declaration.declare_output(name("root.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let mut writer = batch.generated_chunk_writer().unwrap();
        writer.extend_from_slice(b"dropped").unwrap();
        let payload = batch.finish_generated_chunk(writer).unwrap();
        drop(payload);
        let artifact = prepare_resource(&mut batch, &root).unwrap();
        batch.bind_output(output, artifact).unwrap();

        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::Budget(
                ArtifactBudgetError::UnretainedGeneratedBackings { count: 1 }
            ))
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn live_but_unused_generated_payload_cannot_commit_ghost_retention() {
    let root = resource_payload(b"root", 30);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let output = declaration.declare_output(name("root.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let mut writer = batch.generated_chunk_writer().unwrap();
        writer.extend_from_slice(b"unused").unwrap();
        let _unused_payload = batch.finish_generated_chunk(writer).unwrap();
        let artifact = prepare_resource(&mut batch, &root).unwrap();
        batch.bind_output(output, artifact).unwrap();

        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::Budget(
                ArtifactBudgetError::UnretainedGeneratedBackings { count: 1 }
            ))
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn empty_generated_payload_cannot_commit_ghost_retention() {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let output = declaration.declare_output(name("empty.resS")).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let writer = batch.generated_chunk_writer().unwrap();
        let payload = batch.finish_generated_chunk(writer).unwrap();
        let artifact = batch
            .prepare_streamed_resource_extents(
                [StreamedResourceExtentInspection::new(
                    DigestV1::hash_bytes(b""),
                    0,
                    0,
                    1,
                )],
                |encoder| encoder.push_payload_full(&payload),
            )
            .unwrap();
        batch.bind_output(output, artifact).unwrap();

        assert!(matches!(
            batch.finish(),
            Err(ArtifactBuildError::Budget(
                ArtifactBudgetError::UnretainedGeneratedBackings { count: 1 }
            ))
        ));
    }
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn live_generated_writer_prevents_the_no_allocation_commit() {
    let payload = resource_payload(b"root", 13);
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let slot = declaration.declare_output(name("root.resS")).unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let writer = batch.generated_chunk_writer().unwrap();
    let artifact = prepare_resource(&mut batch, &payload).unwrap();
    batch.bind_output(slot, artifact).unwrap();

    assert!(matches!(
        batch.finish(),
        Err(ArtifactBuildError::Budget(
            ArtifactBudgetError::OutstandingTransactionReservations { outstanding: 1 }
        ))
    ));
    drop(writer);
    assert_eq!(
        artifact_budget.committed_usage(),
        ArtifactBudgetUsage::default()
    );
    assert_eq!(artifact_budget.live_scratch_bytes(), 0);
}

#[test]
fn prepared_types_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<PreparedArtifact>();
    assert_send_sync::<PreparedArtifactSet>();
    assert_send_sync::<ArtifactHandle>();
    assert_send_sync::<OutputSlot>();
}
