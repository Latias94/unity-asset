use std::sync::Arc;

use unity_asset_core::{AssetLoadBudget, SourceId, SourceKind, VerifiedSourceImage, WorkspaceId};

use super::*;
use crate::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactBudgetError, ArtifactBudgetUsage,
    ArtifactBuildError, ArtifactLimits, ArtifactPayload, ArtifactPayloadError, LogicalArtifactName,
    PreparedArtifactFormat,
};

fn logical_name(value: &str) -> LogicalArtifactName {
    LogicalArtifactName::new(value).unwrap()
}

fn source_payload(bytes: &[u8], local: u128) -> ArtifactPayload {
    let source = SourceId::new(
        WorkspaceId::from_u128(91).unwrap(),
        SourceKind::StreamedResource,
        local,
    )
    .unwrap();
    let image = VerifiedSourceImage::verify(
        SourceKind::StreamedResource,
        Arc::<[u8]>::from(bytes.to_vec()),
    );
    ArtifactPayload::source_backed(source, image).unwrap()
}

#[test]
fn allocation_is_deterministic_for_multiple_aligned_extents() {
    let extents = [
        StreamedResourceExtent::generated(b"abc", 1).unwrap(),
        StreamedResourceExtent::generated(b"de", 4).unwrap(),
        StreamedResourceExtent::generated(b"", 8).unwrap(),
        StreamedResourceExtent::generated(b"f", 2).unwrap(),
    ];
    let flags = StreamedResourceFlags::new(0x4);
    let plan = StreamedResourcePlan::new(flags, &extents).unwrap();

    assert_eq!(plan.flags(), flags);
    assert_eq!(plan.len(), 9);
    assert_eq!(plan.extent_count(), 4);
    assert_eq!(
        plan.allocations()
            .map(|allocation| {
                (
                    allocation.ordinal(),
                    allocation.offset(),
                    allocation.size(),
                    allocation.alignment(),
                    allocation.padding_before(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (0, 0, 3, 1, 0),
            (1, 4, 2, 4, 1),
            (2, 8, 0, 8, 2),
            (3, 8, 1, 2, 0),
        ]
    );

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let declared = plan
        .declare_output(&mut declaration, logical_name("data/CAB-main.resS"))
        .unwrap();
    let slot = declared.output_slot();
    let mut batch = declaration.seal_output_names().unwrap();
    let prepared = declared.prepare(&mut batch).unwrap();

    assert_eq!(prepared.output_slot(), Some(slot));
    assert_eq!(prepared.flags(), flags);
    assert_eq!(prepared.len(), 9);
    assert_eq!(prepared.extent_count(), 4);

    let set = batch.finish().unwrap();
    let output = set.outputs().next().unwrap();
    assert_eq!(output.name().as_str(), "data/CAB-main.resS");
    assert_eq!(output.handle(), prepared.handle());
    let PreparedArtifactFormat::StreamedResource(inspection) = output.artifact().format() else {
        panic!("resource plan must produce a streamed-resource proof");
    };
    assert_eq!(inspection.length(), 9);
    assert_eq!(inspection.payload_bytes(), 6);
    assert_eq!(inspection.padding_bytes(), 3);
    assert_eq!(
        inspection
            .extents()
            .iter()
            .map(|extent| (
                extent.offset(),
                extent.length(),
                extent.alignment(),
                extent.padding_before(),
            ))
            .collect::<Vec<_>>(),
        vec![(0, 3, 1, 0), (4, 2, 4, 1), (8, 0, 8, 2), (8, 1, 2, 0)]
    );

    let mut bytes = Vec::new();
    let receipt = output.artifact().stream_verified_to(&mut bytes).unwrap();
    assert_eq!(bytes, b"abc\0de\0\0f");
    assert_eq!(receipt.bytes_written(), 9);
    assert_eq!(receipt.digest(), output.artifact().digest());
}

#[test]
fn source_ranges_remain_zero_copy_and_retain_exact_provenance() {
    let payload = source_payload(b"prefixPAYLOADsuffix", 7);
    let extents = [StreamedResourceExtent::artifact_payload_range(&payload, 6..13, 4).unwrap()];
    let plan = StreamedResourcePlan::new(StreamedResourceFlags::default(), &extents).unwrap();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let declared = plan
        .declare_output(&mut declaration, logical_name("CAB-range.resource"))
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    declared.prepare(&mut batch).unwrap();
    let set = batch.finish().unwrap();

    assert_eq!(set.source_dependencies().len(), 1);
    assert_eq!(set.source_dependencies()[0].referenced_bytes(), 7);
    assert_eq!(set.footprint().generated_bytes(), 0);
    assert_eq!(set.footprint().referenced_source_bytes(), 7);

    let output = set.outputs().next().unwrap();
    let mut bytes = Vec::new();
    output.artifact().stream_verified_to(&mut bytes).unwrap();
    assert_eq!(bytes, b"PAYLOAD");
}

#[test]
fn generated_chunk_budget_failure_rolls_back_the_complete_resource_transaction() {
    let extents = [StreamedResourceExtent::generated(b"12345", 1).unwrap()];
    let plan = StreamedResourcePlan::new(StreamedResourceFlags::new(0x4), &extents).unwrap();
    let limits = ArtifactLimits::default().with_max_generated_chunk_bytes(4);
    let mut artifact_budget = ArtifactBudget::new(limits).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();

    {
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
        let declared = plan
            .declare_output(&mut declaration, logical_name("CAB-budget.resS"))
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let error = declared.prepare(&mut batch).unwrap_err();
        assert!(matches!(
            error,
            StreamedResourceError::Artifact(error)
                if matches!(
                    *error,
                    ArtifactBuildError::Payload(ArtifactPayloadError::Budget(
                        ArtifactBudgetError::Exceeded {
                            resource: "generated_chunk_bytes",
                            requested: 5,
                            limit: 4,
                        }
                    ))
                )
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
fn invalid_extent_shape_is_rejected_before_an_artifact_batch_exists() {
    assert_eq!(
        StreamedResourceExtent::generated(b"x", 3).unwrap_err(),
        StreamedResourcePlanError::InvalidAlignment { alignment: 3 }
    );

    let payload = source_payload(b"abc", 9);
    assert_eq!(
        StreamedResourceExtent::artifact_payload_range(&payload, 1..4, 1).unwrap_err(),
        StreamedResourcePlanError::InvalidPayloadRange {
            start: 1,
            end: 4,
            payload_len: 3,
        }
    );
}

#[test]
fn an_empty_sidecar_is_a_valid_exact_artifact() {
    let extents = [];
    let plan = StreamedResourcePlan::new(StreamedResourceFlags::new(12), &extents).unwrap();
    assert_eq!(plan.len(), 0);
    assert!(plan.allocations().next().is_none());

    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let declared = plan
        .declare_output(&mut declaration, logical_name("empty.resource"))
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let prepared = declared.prepare(&mut batch).unwrap();
    assert_eq!(prepared.len(), 0);
    assert_eq!(prepared.extent_count(), 0);
    assert_eq!(prepared.flags().bits(), 12);

    let set = batch.finish().unwrap();
    assert!(set.outputs().next().unwrap().artifact().is_empty());
}
