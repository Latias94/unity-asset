#![cfg(feature = "decode")]

use std::fs;
use std::path::PathBuf;

use image::ImageFormat;
use unity_asset::extraction::{
    ExistingOutputPolicy, ExtractionArtifactKind, ExtractionArtifactStatus,
    ExtractionExecutionLimits, ExtractionExecutionOptions, ExtractionExecutor,
    ExtractionFailurePolicy, ExtractionFilter, ExtractionPlanner, ExtractionRepresentationPolicy,
    ExtractionRequest,
};
use unity_asset::workspace::AssetWorkspace;
use unity_asset::{AssetLoadBudget, DigestV1};
use unity_asset_decode::audio::{AudioCompressionFormat, decode_audio_data};

fn sample(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/samples")
        .join(name)
}

fn options() -> ExtractionExecutionOptions {
    ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(
            2,
            512 * 1024 * 1024,
            4,
            2 * 1024 * 1024 * 1024,
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
    let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
        .with_filter(ExtractionFilter::new([28], None, None, None).unwrap());
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(request, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(plan.artifacts().len(), 1);
    assert_eq!(
        plan.artifacts()[0].preferred_kind(),
        ExtractionArtifactKind::TexturePng
    );

    let directory = tempfile::tempdir().unwrap();
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            directory.path(),
            &options(),
            None,
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
    assert_eq!(artifact.length(), Some(u64::try_from(bytes.len()).unwrap()));
}

#[test]
fn streamed_audio_is_resolved_by_the_plan_and_written_without_filesystem_probing() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("char_118_yuki.ab"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
        .with_filter(ExtractionFilter::new([83], None, None, None).unwrap());
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(request, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(!plan.artifacts().is_empty());
    assert!(
        plan.artifacts()
            .iter()
            .all(|artifact| artifact.preferred_kind() == ExtractionArtifactKind::Audio)
    );

    let directory = tempfile::tempdir().unwrap();
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            directory.path(),
            &options(),
            None,
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
