use std::sync::Arc;

use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, SourceId, SourceKind, VerifiedSourceImage,
    WorkspaceId,
};

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
    let mut incremental_budget = AssetLoadBudget::default();
    let mut planner = StreamedResourcePlanner::new(flags);
    let incremental_allocations = extents
        .iter()
        .cloned()
        .map(|extent| planner.push(extent, &mut incremental_budget).unwrap())
        .collect::<Vec<_>>();
    let incremental_plan = planner.finish();

    assert_eq!(plan.flags(), flags);
    assert_eq!(plan.len(), 9);
    assert_eq!(plan.extent_count(), 4);
    assert_eq!(incremental_plan.flags(), plan.flags());
    assert_eq!(incremental_plan.len(), plan.len());
    assert_eq!(incremental_plan.extent_count(), plan.extent_count());
    assert_eq!(
        incremental_allocations,
        plan.allocations().collect::<Vec<_>>()
    );
    assert_eq!(
        incremental_plan.allocations().collect::<Vec<_>>(),
        plan.allocations().collect::<Vec<_>>()
    );
    assert_eq!(incremental_budget.usage().entries, 4);
    assert!(incremental_budget.usage().bytes > 0);
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
    let declared = incremental_plan
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
fn incremental_budget_failure_keeps_the_completed_prefix_unchanged() {
    let limits = AssetLoadLimits {
        max_entries: 2,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let mut planner = StreamedResourcePlanner::new(StreamedResourceFlags::new(7));

    assert_eq!(
        planner
            .push(
                StreamedResourceExtent::generated(b"abc", 1).unwrap(),
                &mut budget,
            )
            .unwrap()
            .offset(),
        0
    );
    assert_eq!(
        planner
            .push(
                StreamedResourceExtent::generated(b"de", 4).unwrap(),
                &mut budget,
            )
            .unwrap()
            .offset(),
        4
    );
    let before = budget.usage();
    let error = planner
        .push(
            StreamedResourceExtent::generated(b"f", 8).unwrap(),
            &mut budget,
        )
        .unwrap_err();

    assert!(matches!(
        error,
        StreamedResourcePlannerError::Budget(BudgetError::Exceeded {
            resource: "entries",
            requested: 3,
            limit: 2,
        })
    ));
    assert_eq!(budget.usage(), before);
    assert_eq!(planner.extent_count(), 2);
    assert_eq!(planner.len(), 6);
    assert_eq!(
        planner
            .finish()
            .allocations()
            .map(|allocation| (
                allocation.ordinal(),
                allocation.offset(),
                allocation.size(),
                allocation.alignment(),
                allocation.padding_before(),
            ))
            .collect::<Vec<_>>(),
        vec![(0, 0, 3, 1, 0), (1, 4, 2, 4, 1)]
    );
}

#[test]
fn incremental_metadata_allocation_is_preflighted_against_byte_budget() {
    let limits = AssetLoadLimits {
        max_bytes: 1,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let mut planner = StreamedResourcePlanner::new(StreamedResourceFlags::default());
    let error = planner
        .push(
            StreamedResourceExtent::generated(b"x", 1).unwrap(),
            &mut budget,
        )
        .unwrap_err();

    assert!(matches!(
        error,
        StreamedResourcePlannerError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit: 1,
            ..
        })
    ));
    assert_eq!(budget.usage(), Default::default());
    assert!(planner.is_empty());
    assert_eq!(planner.len(), 0);
}

#[test]
fn incremental_growth_budget_failure_preserves_the_retained_prefix() {
    let mut budget = AssetLoadBudget::default();
    let mut planner = StreamedResourcePlanner::new(StreamedResourceFlags::default());
    planner
        .push(
            StreamedResourceExtent::generated(b"x", 1).unwrap(),
            &mut budget,
        )
        .unwrap();

    // Leave one byte for unrelated caller work. Existing capacity remains usable, but the next
    // metadata growth must fail before it moves any retained extent.
    let supplemental = budget.remaining_bytes().checked_sub(1).unwrap();
    budget.consume_bytes(supplemental).unwrap();
    let mut completed = 1;
    loop {
        let before = budget.usage();
        match planner.push(
            StreamedResourceExtent::generated(b"x", 1).unwrap(),
            &mut budget,
        ) {
            Ok(allocation) => {
                assert_eq!(allocation.ordinal(), completed);
                assert_eq!(allocation.offset(), u64::try_from(completed).unwrap());
                assert_eq!(budget.usage().bytes, before.bytes);
                completed += 1;
            }
            Err(StreamedResourcePlannerError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })) => {
                assert_eq!(budget.usage(), before);
                break;
            }
            Err(error) => panic!("unexpected planner failure: {error}"),
        }
    }

    assert!(completed >= 4);
    assert_eq!(planner.extent_count(), completed);
    assert_eq!(planner.len(), u64::try_from(completed).unwrap());
    assert_eq!(planner.finish().allocations().count(), completed);
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

    let mut planner = StreamedResourcePlanner::new(StreamedResourceFlags::new(12));
    let mut planner_budget = AssetLoadBudget::default();
    let empty_allocation = planner
        .push(
            StreamedResourceExtent::generated(b"", 8).unwrap(),
            &mut planner_budget,
        )
        .unwrap();
    assert_eq!(empty_allocation.offset(), 0);
    assert!(planner.is_empty());
    assert_eq!(planner.extent_count(), 1);
    assert!(planner.finish().is_empty());

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
