#![cfg(feature = "decode")]

use std::fs;
use std::path::PathBuf;

use image::ImageFormat;
use unity_asset::extraction::{
    EXTRACTION_PLAN_VERSION, ExistingOutputPolicy, ExtractionArtifactKind,
    ExtractionArtifactStatus, ExtractionDiagnosticCode, ExtractionExecutionError,
    ExtractionExecutionLimits, ExtractionExecutionOptions, ExtractionExecutor,
    ExtractionFailurePolicy, ExtractionFilter, ExtractionPlan, ExtractionPlanError,
    ExtractionPlanMismatchKind, ExtractionPlanner, ExtractionRepresentationPolicy,
    ExtractionRequest, ExtractionRunOptions,
};
use unity_asset::workspace::{AssetWorkspace, WorkspaceError};
use unity_asset::{AssetLoadBudget, AssetLoadLimits, BudgetError, DigestV1};
use unity_asset_decode::audio::{AudioCompressionFormat, decode_audio_data};

fn sample(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/samples")
        .join(name)
}

fn options() -> ExtractionExecutionOptions {
    options_with_output_limit(2 * 1024 * 1024 * 1024)
}

fn options_with_output_limit(max_output_bytes: u64) -> ExtractionExecutionOptions {
    ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(
            2,
            512 * 1024 * 1024,
            5,
            max_output_bytes,
            u64::MAX,
            16 * 1024 * 1024,
        )
        .unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap()
}

fn final_ogg_granule(bytes: &[u8]) -> u64 {
    let mut cursor = 0_usize;
    let mut end_granule = None;
    while cursor < bytes.len() {
        let header = bytes
            .get(cursor..cursor + 27)
            .expect("every Ogg page must contain its fixed header");
        assert_eq!(&header[..4], b"OggS");
        let segment_count = usize::from(header[26]);
        let segment_table = bytes
            .get(cursor + 27..cursor + 27 + segment_count)
            .expect("every Ogg page must contain its segment table");
        let payload_len = segment_table
            .iter()
            .map(|length| usize::from(*length))
            .sum::<usize>();
        if header[5] & 0x04 != 0 {
            end_granule = Some(u64::from_le_bytes(header[6..14].try_into().unwrap()));
        }
        cursor = cursor
            .checked_add(27 + segment_count + payload_len)
            .expect("Ogg page length must not overflow");
        assert!(cursor <= bytes.len(), "Ogg page exceeds the artifact");
    }
    assert_eq!(cursor, bytes.len());
    end_granule.expect("rebuilt Ogg stream must contain an EOS page")
}

#[test]
fn banner_texture_exports_as_revision_bound_png() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("banner_1"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let planner = ExtractionPlanner::new(&snapshot);
    let request = || {
        ExtractionRequest::all(ExtractionRepresentationPolicy::PreferDecoded)
            .with_filter(ExtractionFilter::new([28], None, None, None).unwrap())
    };
    let cold_plan = planner
        .plan(request(), &mut AssetLoadBudget::default())
        .unwrap();
    let mut measured = AssetLoadBudget::default();
    let plan = planner.plan(request(), &mut measured).unwrap();
    assert_eq!(plan, cold_plan);
    assert_eq!(plan.artifacts().len(), 1);
    assert_eq!(
        plan.artifacts()[0].preferred_kind(),
        ExtractionArtifactKind::TexturePng
    );
    assert_eq!(
        plan.artifacts()[0].fallback_kind(),
        Some(ExtractionArtifactKind::BinaryRaw)
    );
    let usage = measured.usage();
    assert!(usage.bytes > 1);
    let exact_limits = AssetLoadLimits {
        max_entries: usage.entries.max(1),
        max_bytes: usage.bytes,
        max_depth: usage.max_observed_depth,
        max_members: usage.members.max(1),
        max_compressed_bytes: usage.compressed_bytes.max(1),
        max_decompressed_bytes: usage.decompressed_bytes.max(1),
        ..AssetLoadLimits::default()
    };
    let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
    assert_eq!(planner.plan(request(), &mut exact).unwrap(), plan);
    assert_eq!(exact.usage(), usage);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..exact_limits
    })
    .unwrap();
    let error = planner.plan(request(), &mut one_short).unwrap_err();
    assert!(matches!(
        error,
        ExtractionPlanError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }) | ExtractionPlanError::Workspace(WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));

    let directory = tempfile::tempdir().unwrap();
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            directory.path(),
            ExtractionRunOptions::new(options()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let artifact = &report.manifest().artifacts()[0];
    assert_eq!(
        artifact.status(),
        ExtractionArtifactStatus::Written,
        "decoded texture artifact did not publish: {artifact:#?}"
    );
    let bytes = fs::read(directory.path().join(artifact.path().as_str())).unwrap();

    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    let image = image::load_from_memory_with_format(&bytes, ImageFormat::Png)
        .expect("exported banner must decode as PNG")
        .to_rgba8();
    assert_eq!(image.dimensions(), (492, 180));
    assert!(image.pixels().any(|pixel| pixel[3] != 0));
    assert_eq!(artifact.digest(), Some(DigestV1::hash_bytes(&bytes)));
    let output_length = u64::try_from(bytes.len()).unwrap();
    assert_eq!(artifact.length(), Some(output_length));

    let strict_plan = planner
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
                .with_filter(ExtractionFilter::new([28], None, None, None).unwrap()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let exact_directory = tempfile::tempdir().unwrap();
    let exact = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &strict_plan,
            exact_directory.path(),
            ExtractionRunOptions::new(options_with_output_limit(output_length)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(exact.counts().written(), 1);

    let one_short_directory = tempfile::tempdir().unwrap();
    let one_short = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &strict_plan,
            one_short_directory.path(),
            ExtractionRunOptions::new(options_with_output_limit(output_length - 1)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(one_short.counts().failed(), 1);
    let failed = &one_short.manifest().artifacts()[0];
    assert_eq!(
        failed.diagnostics()[0].code(),
        ExtractionDiagnosticCode::OutputLimitExceeded
    );
    assert!(
        !one_short_directory
            .path()
            .join(failed.path().as_str())
            .exists()
    );
}

#[test]
fn streamed_audio_is_resolved_by_the_plan_and_written_without_filesystem_probing() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("char_118_yuki.ab"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let planner = ExtractionPlanner::new(&snapshot);
    let request = || {
        ExtractionRequest::all(ExtractionRepresentationPolicy::PreferDecoded)
            .with_filter(ExtractionFilter::new([83], None, None, None).unwrap())
    };
    let cold_plan = planner
        .plan(request(), &mut AssetLoadBudget::default())
        .unwrap();
    let mut measured = AssetLoadBudget::default();
    let plan = planner.plan(request(), &mut measured).unwrap();
    assert_eq!(plan, cold_plan);
    assert!(!plan.artifacts().is_empty());
    assert!(
        plan.artifacts()
            .iter()
            .all(|artifact| artifact.preferred_kind() == ExtractionArtifactKind::Audio)
    );
    assert!(
        plan.artifacts().iter().all(|artifact| {
            artifact.fallback_kind() == Some(ExtractionArtifactKind::BinaryRaw)
        })
    );
    let usage = measured.usage();
    assert!(usage.bytes > 1);
    let exact_limits = AssetLoadLimits {
        max_entries: usage.entries.max(1),
        max_bytes: usage.bytes,
        max_depth: usage.max_observed_depth,
        max_members: usage.members.max(1),
        max_compressed_bytes: usage.compressed_bytes.max(1),
        max_decompressed_bytes: usage.decompressed_bytes.max(1),
        ..AssetLoadLimits::default()
    };
    let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
    let exact_plan = planner.plan(request(), &mut exact).unwrap();
    assert_eq!(exact_plan, plan);
    assert_eq!(exact.usage(), usage);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..exact_limits
    })
    .unwrap();
    let error = planner.plan(request(), &mut one_short).unwrap_err();
    assert!(matches!(
        error,
        ExtractionPlanError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }) | ExtractionPlanError::Workspace(WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));

    let directory = tempfile::tempdir().unwrap();
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            directory.path(),
            ExtractionRunOptions::new(options()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    for artifact in report.manifest().artifacts() {
        assert_eq!(
            artifact.status(),
            ExtractionArtifactStatus::Written,
            "decoded audio artifact did not publish: {artifact:#?}"
        );
        let bytes = fs::read(directory.path().join(artifact.path().as_str())).unwrap();
        assert_eq!(&bytes[..4], b"OggS");
        let decoded = decode_audio_data(AudioCompressionFormat::Vorbis, bytes.clone())
            .expect("rebuilt FSB5 audio must be a playable Ogg/Vorbis stream");
        assert!(decoded.sample_count() > 0);
        assert_eq!(
            u64::try_from(decoded.frame_count()).unwrap(),
            final_ogg_granule(&bytes),
            "the decoder must trim the final Vorbis block to the FSB5-declared frame count"
        );
        assert_eq!(artifact.digest(), Some(DigestV1::hash_bytes(&bytes)));
    }
}

#[test]
fn media_plan_rejects_a_destination_suffix_that_disagrees_with_its_descriptor() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("banner_1"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
                .with_filter(ExtractionFilter::new([28], None, None, None).unwrap()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut encoded = serde_json::to_value(plan).unwrap();
    assert_eq!(
        encoded["version"],
        serde_json::json!(EXTRACTION_PLAN_VERSION)
    );
    let path = encoded["artifacts"][0]["preferred_path"]
        .as_str()
        .unwrap()
        .to_owned();
    encoded["artifacts"][0]["preferred_path"] =
        serde_json::Value::String(path.strip_suffix(".png").unwrap().to_owned() + ".bin");

    let error = serde_json::from_value::<ExtractionPlan>(encoded).unwrap_err();

    assert!(
        error.to_string().contains("canonical .png suffix"),
        "{error}"
    );
}

#[test]
fn execution_reprepares_media_and_rejects_descriptor_drift_before_staging() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("banner_1"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
                .with_filter(ExtractionFilter::new([28], None, None, None).unwrap()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut encoded = serde_json::to_value(plan).unwrap();
    let output_bound =
        encoded["artifacts"][0]["preferred_content"]["descriptor"]["output"]["upper_bound"]
            .as_u64()
            .unwrap();
    encoded["artifacts"][0]["preferred_content"]["descriptor"]["output"]["upper_bound"] =
        serde_json::Value::from(output_bound + 1);
    let plan = serde_json::from_value::<ExtractionPlan>(encoded).unwrap();
    let destination = plan.artifacts()[0].preferred_path().as_str().to_owned();
    let directory = tempfile::tempdir().unwrap();

    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            directory.path(),
            ExtractionRunOptions::new(options()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ExtractionExecutionError::PlanVerification(source)
            if matches!(
                source.as_ref(),
                ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::Representations,
                }
            )
    ));
    assert!(!directory.path().join(destination).exists());
}
