use std::io::{Read as _, Seek as _, SeekFrom};
use std::ops::Range;
use std::sync::Arc;

use unity_asset_core::{
    AssetLoadBudget, DigestV1, SourceFingerprint, SourceId, SourceKind, UnityClass, UnityDocument,
    VerifiedSourceImage, WorkspaceId,
};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactBuildError, ArtifactHandle, ArtifactLimits,
    ArtifactPayload, LogicalArtifactName, PreparedArtifactKind, PreparedArtifactSet,
    PreparedArtifactSourceCompatibilityError,
};
use unity_asset_write::resources::{
    StreamedResourceExtent, StreamedResourceFlags, StreamedResourcePlan,
};
use unity_asset_yaml::YamlDocument;

use super::{WorkspaceByteRange, WorkspaceError, WorkspaceYamlObject};

struct PreparedFixture {
    artifacts: Arc<PreparedArtifactSet>,
    handle: ArtifactHandle,
    source: SourceId,
    fingerprint: SourceFingerprint,
}

fn compatibility_error(error: &WorkspaceError) -> &PreparedArtifactSourceCompatibilityError {
    let WorkspaceError::PreparedArtifactSourceCompatibility(source) = error else {
        panic!("expected prepared artifact source compatibility error");
    };
    source.as_ref()
}

fn source_payload(workspace: WorkspaceId, local: u128, bytes: &'static [u8]) -> ArtifactPayload {
    let source = SourceId::new(workspace, SourceKind::StreamedResource, local).unwrap();
    let image = VerifiedSourceImage::verify(SourceKind::StreamedResource, Arc::<[u8]>::from(bytes));
    ArtifactPayload::source_backed(source, image).unwrap()
}

fn segmented_fixture(workspace_local: u128) -> PreparedFixture {
    let workspace = WorkspaceId::from_u128(workspace_local).unwrap();
    let first = source_payload(workspace, 1, b"abc");
    let second = source_payload(workspace, 2, b"def");
    let extents = [
        StreamedResourceExtent::artifact_payload(&first, 1).unwrap(),
        StreamedResourceExtent::artifact_payload(&second, 1).unwrap(),
    ];
    let plan = StreamedResourcePlan::new(StreamedResourceFlags::default(), &extents).unwrap();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let declared = plan
        .declare_output(
            &mut declaration,
            LogicalArtifactName::new("segmented.resS").unwrap(),
        )
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let handle = declared.prepare(&mut batch).unwrap().handle();
    let artifacts = Arc::new(batch.finish().unwrap());
    let digest = artifacts.artifact(handle).unwrap().digest();
    let source = SourceId::new(workspace, SourceKind::StreamedResource, 3).unwrap();

    PreparedFixture {
        artifacts,
        handle,
        source,
        fingerprint: SourceFingerprint::new(SourceKind::StreamedResource, digest),
    }
}

fn verbatim_fixture(workspace_local: u128) -> PreparedFixture {
    let workspace = WorkspaceId::from_u128(workspace_local).unwrap();
    let source = SourceId::new(workspace, SourceKind::AssetBundle, 1).unwrap();
    let image = VerifiedSourceImage::verify(
        SourceKind::AssetBundle,
        Arc::<[u8]>::from(b"verbatim bundle".as_slice()),
    );
    let fingerprint = image.fingerprint();
    let payload = ArtifactPayload::source_backed(source, image).unwrap();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(LogicalArtifactName::new("verbatim.bundle").unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let handle = batch.prepare_verbatim_source(&payload).unwrap();
    batch.bind_output(output, handle).unwrap();

    PreparedFixture {
        artifacts: Arc::new(batch.finish().unwrap()),
        handle,
        source,
        fingerprint,
    }
}

fn byte_range(start: u64, end: u64) -> Range<u64> {
    Range { start, end }
}

#[test]
fn committed_yaml_object_borrows_its_document_class_without_cloning_it() {
    let document = YamlDocument::from_entries(vec![UnityClass::new(
        1,
        "GameObject".to_owned(),
        "1001".to_owned(),
    )]);
    let document = Arc::new(document);
    let expected_class = std::ptr::from_ref(&document.entries()[0]);

    let object = WorkspaceYamlObject::new(Arc::clone(&document), 0);
    let cloned = object.clone();

    assert_eq!(object.document_index(), 0);
    assert_eq!(std::ptr::from_ref(object.class()), expected_class);
    assert_eq!(std::ptr::from_ref(cloned.class()), expected_class);
    assert_eq!(Arc::strong_count(&document), 3);
}

#[test]
fn yaml_object_debug_is_bounded_and_omits_sibling_values() {
    let class = UnityClass::with_properties(
        114,
        "MonoBehaviour".to_owned(),
        "4201".to_owned(),
        indexmap::IndexMap::from([(
            "m_Secret".to_owned(),
            "selected-object-sensitive-property".into(),
        )]),
    );
    let sibling = UnityClass::with_properties(
        1,
        "SiblingObject".to_owned(),
        "9001".to_owned(),
        indexmap::IndexMap::from([("m_Secret".to_owned(), "sibling-sensitive-property".into())]),
    );
    let document = YamlDocument::from_entries(vec![class, sibling]);

    let committed = WorkspaceYamlObject::new(Arc::new(document), 0);
    let committed_debug = format!("{committed:?}");

    assert_eq!(
        committed_debug,
        "WorkspaceYamlObject { document_index: 0, class_id: 114, class_name: \"MonoBehaviour\", anchor: \"4201\" }"
    );
    for sensitive in [
        "m_Secret",
        "selected-object-sensitive-property",
        "SiblingObject",
        "sibling-sensitive-property",
    ] {
        assert!(!committed_debug.contains(sensitive));
    }
}

#[test]
fn prepared_range_rejects_foreign_artifact_capabilities() {
    let fixture = segmented_fixture(0x501);
    let foreign = segmented_fixture(0x502);

    let error = WorkspaceByteRange::from_prepared(
        fixture.source,
        fixture.fingerprint,
        Arc::clone(&fixture.artifacts),
        foreign.handle,
        0..1,
    )
    .unwrap_err();

    let WorkspaceError::PreparedArtifact(error) = error else {
        panic!("foreign handles must retain the typed artifact error");
    };
    assert!(matches!(
        *error,
        ArtifactBuildError::ForeignArtifactHandle { .. }
    ));
}

#[test]
fn prepared_range_rejects_source_fingerprint_and_artifact_kind_mismatches() {
    let fixture = segmented_fixture(0x503);
    let fingerprint_kind_error = WorkspaceByteRange::from_prepared(
        fixture.source,
        SourceFingerprint::new(SourceKind::Yaml, fixture.fingerprint.digest()),
        Arc::clone(&fixture.artifacts),
        fixture.handle,
        0..1,
    )
    .unwrap_err();
    assert!(matches!(
        compatibility_error(&fingerprint_kind_error),
        PreparedArtifactSourceCompatibilityError::FingerprintKindMismatch {
                source_id,
                fingerprint_kind: SourceKind::Yaml,
            } if *source_id == fixture.source
    ));

    let yaml_source = SourceId::new(fixture.source.workspace(), SourceKind::Yaml, 4).unwrap();
    let artifact_kind_error = WorkspaceByteRange::from_prepared(
        yaml_source,
        SourceFingerprint::new(SourceKind::Yaml, fixture.fingerprint.digest()),
        Arc::clone(&fixture.artifacts),
        fixture.handle,
        0..1,
    )
    .unwrap_err();
    assert!(matches!(
        compatibility_error(&artifact_kind_error),
        PreparedArtifactSourceCompatibilityError::ArtifactKindMismatch {
            source_id,
            source_kind: SourceKind::Yaml,
            artifact_kind: PreparedArtifactKind::StreamedResource,
        } if *source_id == yaml_source
    ));
}

#[test]
fn prepared_range_rejects_digest_and_range_mismatches() {
    let fixture = segmented_fixture(0x504);
    let wrong_digest = DigestV1::hash_bytes(b"not the prepared artifact");
    assert_ne!(wrong_digest, fixture.fingerprint.digest());
    let digest_error = WorkspaceByteRange::from_prepared(
        fixture.source,
        SourceFingerprint::new(SourceKind::StreamedResource, wrong_digest),
        Arc::clone(&fixture.artifacts),
        fixture.handle,
        0..1,
    )
    .unwrap_err();
    assert!(matches!(
        compatibility_error(&digest_error),
        PreparedArtifactSourceCompatibilityError::DigestMismatch {
            source_id,
            expected,
            actual,
        } if *source_id == fixture.source
            && *expected == wrong_digest
            && *actual == fixture.fingerprint.digest()
    ));

    for (range, expected_offset, expected_end) in
        [(byte_range(0, 7), 0, 7), (byte_range(5, 4), 5, 4)]
    {
        let range_error = WorkspaceByteRange::from_prepared(
            fixture.source,
            fixture.fingerprint,
            Arc::clone(&fixture.artifacts),
            fixture.handle,
            range,
        )
        .unwrap_err();
        assert!(matches!(
            range_error,
            WorkspaceError::RangeOutOfBounds {
                source_id,
                offset,
                end,
                source_len: 6,
            } if source_id == fixture.source
                && offset == expected_offset
                && end == expected_end
        ));
    }
}

#[test]
fn prepared_verbatim_range_rejects_source_and_fingerprint_provenance_mismatches() {
    let fixture = verbatim_fixture(0x506);
    let foreign_source = SourceId::new(
        fixture.source.workspace(),
        SourceKind::AssetBundle,
        fixture.source.local() + 1,
    )
    .unwrap();
    let source_error = WorkspaceByteRange::from_prepared(
        foreign_source,
        fixture.fingerprint,
        Arc::clone(&fixture.artifacts),
        fixture.handle,
        0..1,
    )
    .unwrap_err();
    assert!(matches!(
        compatibility_error(&source_error),
        PreparedArtifactSourceCompatibilityError::VerbatimSourceMismatch {
            expected,
            actual,
        } if *expected == foreign_source && *actual == fixture.source
    ));

    let foreign_fingerprint = SourceFingerprint::from_bytes(SourceKind::AssetBundle, b"other");
    let fingerprint_error = WorkspaceByteRange::from_prepared(
        fixture.source,
        foreign_fingerprint,
        Arc::clone(&fixture.artifacts),
        fixture.handle,
        0..1,
    )
    .unwrap_err();
    assert!(matches!(
        compatibility_error(&fingerprint_error),
        PreparedArtifactSourceCompatibilityError::VerbatimFingerprintMismatch {
            expected,
            actual,
        } if *expected == foreign_fingerprint
            && *actual == fixture.fingerprint
    ));
}

#[test]
fn prepared_empty_ranges_are_stable_at_every_segment_boundary() {
    for boundary in [0_u64, 3, 6] {
        let fixture = segmented_fixture(0x507 + u128::from(boundary));
        let range = WorkspaceByteRange::from_prepared(
            fixture.source,
            fixture.fingerprint,
            Arc::clone(&fixture.artifacts),
            fixture.handle,
            boundary..boundary,
        )
        .unwrap();
        drop(fixture);

        assert_eq!(range.contiguous(), Some(b"".as_slice()));
        assert_eq!(range.len(), 0);
        assert!(range.is_empty());
        assert_eq!(range.reader().read(&mut [0_u8; 1]).unwrap(), 0);
        assert_eq!(range.reader().seek(SeekFrom::Start(0)).unwrap(), 0);
        assert_eq!(range.reader().seek(SeekFrom::End(0)).unwrap(), 0);
        let mut copied = Vec::new();
        assert_eq!(range.copy_to(&mut copied).unwrap(), 0);
        assert!(copied.is_empty());
    }
}

#[test]
fn prepared_range_reads_copies_and_seeks_across_backing_segments() {
    let fixture = segmented_fixture(0x505);
    let source = fixture.source;
    let fingerprint = fixture.fingerprint;
    let range = WorkspaceByteRange::from_prepared(
        source,
        fingerprint,
        Arc::clone(&fixture.artifacts),
        fixture.handle,
        1..5,
    )
    .unwrap();
    let retained = range.clone();
    drop(fixture);

    assert_eq!(range.source(), source);
    assert_eq!(range.fingerprint(), fingerprint);
    assert_eq!(range.len(), 4);
    assert!(!range.is_empty());
    assert!(range.contiguous().is_none());

    let mut all = Vec::new();
    range.reader().read_to_end(&mut all).unwrap();
    assert_eq!(all, b"bcde");

    let mut copied = Vec::new();
    assert_eq!(range.copy_to(&mut copied).unwrap(), 4);
    assert_eq!(copied, b"bcde");

    let mut reader = range.reader();
    assert_eq!(reader.seek(SeekFrom::Start(1)).unwrap(), 1);
    let mut crossing = [0_u8; 2];
    reader.read_exact(&mut crossing).unwrap();
    assert_eq!(&crossing, b"cd");
    assert_eq!(reader.seek(SeekFrom::Current(-2)).unwrap(), 1);
    let mut current = [0_u8; 1];
    reader.read_exact(&mut current).unwrap();
    assert_eq!(&current, b"c");
    assert_eq!(reader.seek(SeekFrom::End(-1)).unwrap(), 3);
    reader.read_exact(&mut current).unwrap();
    assert_eq!(&current, b"e");

    let end_error = reader.seek(SeekFrom::End(1)).unwrap_err();
    assert_eq!(end_error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(reader.seek(SeekFrom::Start(2)).unwrap(), 2);
    let underflow = reader.seek(SeekFrom::Current(i64::MIN)).unwrap_err();
    assert_eq!(underflow.kind(), std::io::ErrorKind::InvalidInput);
    reader.read_exact(&mut current).unwrap();
    assert_eq!(&current, b"d");

    drop(range);
    let mut retained_bytes = Vec::new();
    retained.reader().read_to_end(&mut retained_bytes).unwrap();
    assert_eq!(retained_bytes, b"bcde");
}
