use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tempfile::tempdir;
use unity_asset_binary::asset::SerializedFileParser;
use unity_asset_binary::object::ObjectPayloadProvenance;
use unity_asset_binary::shared_bytes::SharedBytes;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, ContractError, DigestV1, SourceAlias,
    SourceFingerprint, SourceKind, UnityValue, VerifiedSourceImage, arc_slice_allocation_bytes,
};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload, LogicalArtifactName,
};
use unity_asset_write::serialized_file::{
    SerializedFileEdits, SerializedFileSource, SerializedFileWriter,
};

use crate::reference::{RawReferenceTarget, ReferenceGraphBuildOptions, ReferenceResolution};

use super::*;
use crate::workspace::{AssetWorkspace, SourceOpenRequest};

struct TestAllocator;

#[global_allocator]
static TEST_ALLOCATOR: TestAllocator = TestAllocator;
static ALLOCATION_TRACKING_ACTIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    static TRACKED_ALLOCATION_THRESHOLD: Cell<usize> = const { Cell::new(usize::MAX) };
    static TRACKED_ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

fn record_tracked_allocation(size: usize) {
    if !ALLOCATION_TRACKING_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let _ = TRACKED_ALLOCATION_THRESHOLD.try_with(|threshold| {
        if size >= threshold.get() {
            let _ = TRACKED_ALLOCATION_COUNT.try_with(|count| {
                count.set(count.get().saturating_add(1));
            });
        }
    });
}

// SAFETY: every operation delegates to `System` with the original allocation contract unchanged.
unsafe impl GlobalAlloc for TestAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: this allocator preserves `System`'s allocation contract and only observes size.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_tracked_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: this allocator preserves `System`'s allocation contract and only observes size.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_tracked_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` and `layout` came from the delegated `System` allocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `pointer` and `layout` came from the delegated `System` allocator.
        let pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !pointer.is_null() {
            record_tracked_allocation(new_size);
        }
        pointer
    }
}

struct AllocationTrackingGuard;

impl Drop for AllocationTrackingGuard {
    fn drop(&mut self) {
        ALLOCATION_TRACKING_ACTIVE.store(false, Ordering::Relaxed);
        let _ = TRACKED_ALLOCATION_THRESHOLD.try_with(|threshold| threshold.set(usize::MAX));
    }
}

fn count_allocations_at_least<T>(threshold: usize, operation: impl FnOnce() -> T) -> (T, usize) {
    assert!(threshold > 0);
    TRACKED_ALLOCATION_COUNT.with(|count| count.set(0));
    TRACKED_ALLOCATION_THRESHOLD.with(|tracked| tracked.set(threshold));
    ALLOCATION_TRACKING_ACTIVE.store(true, Ordering::Relaxed);
    let guard = AllocationTrackingGuard;
    let result = operation();
    let count = TRACKED_ALLOCATION_COUNT.with(Cell::get);
    drop(guard);
    (result, count)
}

const BASE_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Target: {fileID: 0}
--- !u!1 &2
GameObject:
  m_Target: {fileID: 0}
"#;

const PREPARED_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Target: {fileID: 2}
--- !u!1 &2
GameObject:
  m_Target: {fileID: 0}
"#;

const MISMATCHED_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Target: {fileID: 0}
--- !u!1 &3
GameObject:
  m_Target: {fileID: 0}
"#;

const V22_SERIALIZED_FILE: &[u8] = include_bytes!(
    "../../../../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin"
);

fn loaded_yaml_workspace() -> (tempfile::TempDir, AssetWorkspace, SourceId) {
    let directory = tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, BASE_YAML).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let source = workspace
        .load_source(
            SourceOpenRequest::new(path, SourceAlias::new("scene.prefab").unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    (directory, workspace, source)
}

fn prepared_yaml_artifact(
    source: SourceId,
) -> (Arc<PreparedArtifactSet>, ArtifactHandle, SourceFingerprint) {
    prepared_yaml_artifact_from(source, PREPARED_YAML)
}

fn prepared_yaml_artifact_from(
    source: SourceId,
    contents: &str,
) -> (Arc<PreparedArtifactSet>, ArtifactHandle, SourceFingerprint) {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(LogicalArtifactName::new("scene.prefab").unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let mut writer = batch.yaml_writer().unwrap();
    writer.write_all(contents.as_bytes()).unwrap();
    let handle = batch.prepare_yaml_writer(writer).unwrap();
    batch.bind_output(output, handle).unwrap();
    let artifacts = Arc::new(batch.finish().unwrap());
    let fingerprint =
        SourceFingerprint::new(source.kind(), artifacts.artifact(handle).unwrap().digest());
    (artifacts, handle, fingerprint)
}

fn prepared_yaml_artifact_with_orphan(
    source: SourceId,
) -> (Arc<PreparedArtifactSet>, ArtifactHandle, SourceFingerprint) {
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let scene_output = declaration
        .declare_output(LogicalArtifactName::new("scene.prefab").unwrap())
        .unwrap();
    let orphan_output = declaration
        .declare_output(LogicalArtifactName::new("orphan.prefab").unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let mut scene_writer = batch.yaml_writer().unwrap();
    scene_writer.write_all(PREPARED_YAML.as_bytes()).unwrap();
    let scene = batch.prepare_yaml_writer(scene_writer).unwrap();
    let mut orphan_writer = batch.yaml_writer().unwrap();
    orphan_writer.write_all(BASE_YAML.as_bytes()).unwrap();
    let orphan = batch.prepare_yaml_writer(orphan_writer).unwrap();
    batch.bind_output(scene_output, scene).unwrap();
    batch.bind_output(orphan_output, orphan).unwrap();
    let artifacts = Arc::new(batch.finish().unwrap());
    let fingerprint =
        SourceFingerprint::new(source.kind(), artifacts.artifact(scene).unwrap().digest());
    (artifacts, scene, fingerprint)
}

fn prepared_view() -> (tempfile::TempDir, WorkspaceSnapshot, PreparedView, SourceId) {
    let (directory, workspace, source) = loaded_yaml_workspace();
    let snapshot = workspace.snapshot();
    let (artifacts, artifact, fingerprint) = prepared_yaml_artifact(source);
    let mut budget = AssetLoadBudget::default();
    let mut catalog = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut budget)
        .unwrap();
    catalog
        .replace_fingerprint(source, fingerprint, &mut budget)
        .unwrap();
    let catalog = catalog.commit(&mut budget).unwrap();
    let state = PreparedState::new(
        snapshot.clone(),
        catalog,
        DigestV1::hash_bytes(b"prepared-view-test-plan"),
        artifacts,
        vec![PreparedSourceBinding::new(source, fingerprint, artifact)],
        &mut budget,
    )
    .unwrap();
    let view = PreparedView::new(state);
    (directory, snapshot, view, source)
}

#[test]
fn prepared_view_projects_one_candidate_revision_across_all_queries() {
    let (_directory, snapshot, view, source) = prepared_view();
    assert_eq!(view.workspace_id(), snapshot.workspace_id());
    assert_eq!(view.base_revision(), snapshot.revision());
    assert_ne!(view.revision(), snapshot.revision());

    let projected = WorkspaceView::source(&view, source, &mut AssetLoadBudget::default()).unwrap();
    let WorkspaceLookup::Resolved(projected) = projected else {
        panic!("prepared source must resolve");
    };
    assert_eq!(
        projected.fingerprint(),
        view.state().catalog().fingerprint(source).unwrap()
    );
    assert!(projected.physical_origin().is_some());

    let objects = WorkspaceView::objects(&view, &mut AssetLoadBudget::default()).unwrap();
    assert!(
        objects
            .iter()
            .all(|handle| handle.revision() == view.revision())
    );
    let first = objects
        .iter()
        .find(|handle| handle.object().yaml_anchor() == Some("1"))
        .unwrap();
    let second = objects
        .iter()
        .find(|handle| handle.object().yaml_anchor() == Some("2"))
        .unwrap();
    let object = WorkspaceView::read_object(&view, first, &mut AssetLoadBudget::default()).unwrap();
    assert_eq!(
        object
            .class()
            .get("m_Target")
            .and_then(|value| value.as_object())
            .and_then(|target| target.get("fileID"))
            .and_then(UnityValue::as_i64),
        Some(2)
    );

    let base_error =
        WorkspaceView::read_object(&snapshot, first, &mut AssetLoadBudget::default()).unwrap_err();
    assert!(matches!(
        base_error,
        WorkspaceError::Contract(ContractError::RevisionMismatch { .. })
    ));
    let old_handle = WorkspaceView::objects(&snapshot, &mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let prepared_error =
        WorkspaceView::read_object(&view, &old_handle, &mut AssetLoadBudget::default())
            .unwrap_err();
    assert!(matches!(
        prepared_error,
        WorkspaceError::Contract(ContractError::RevisionMismatch { .. })
    ));

    let base_graph = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared_graph = view.reference_graph();
    assert!(std::ptr::eq(prepared_graph, view.reference_graph()));
    assert_eq!(prepared_graph.revision(), view.revision());
    let base_fact = base_graph
        .facts()
        .iter()
        .find(|fact| fact.source().object().yaml_anchor() == Some("1"))
        .unwrap();
    assert!(matches!(
        base_fact.raw_target(),
        RawReferenceTarget::Yaml {
            file_id: Some(0),
            ..
        }
    ));
    let prepared_fact = prepared_graph
        .facts()
        .iter()
        .find(|fact| fact.source().object().yaml_anchor() == Some("1"))
        .unwrap();
    assert!(matches!(
        prepared_fact.raw_target(),
        RawReferenceTarget::Yaml {
            file_id: Some(2),
            ..
        }
    ));
    assert!(matches!(
        prepared_fact.resolution(),
        ReferenceResolution::Resolved(target)
            if *target == *second && target.revision() == view.revision()
    ));
}

#[test]
fn prepared_source_range_reads_exact_artifact_bytes_without_changing_the_baseline() {
    let (_directory, snapshot, view, source) = prepared_view();
    let prepared_range = WorkspaceView::read_source_range(
        &view,
        source,
        0,
        u64::try_from(PREPARED_YAML.len()).unwrap(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let mut range = prepared_range.reader();
    let mut prepared = String::new();
    range.read_to_string(&mut prepared).unwrap();
    assert_eq!(prepared, PREPARED_YAML);

    let baseline_range = WorkspaceView::read_source_range(
        &snapshot,
        source,
        0,
        u64::try_from(BASE_YAML.len()).unwrap(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let mut baseline = baseline_range.reader();
    let mut original = String::new();
    baseline.read_to_string(&mut original).unwrap();
    assert_eq!(original, BASE_YAML);
}

#[test]
fn prepared_state_rejects_yaml_identity_mismatch_from_the_exact_artifact() {
    let (_directory, workspace, source) = loaded_yaml_workspace();
    let snapshot = workspace.snapshot();
    let (artifacts, artifact, fingerprint) = prepared_yaml_artifact_from(source, MISMATCHED_YAML);
    let mut catalog_budget = AssetLoadBudget::default();
    let mut transaction = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut catalog_budget)
        .unwrap();
    transaction
        .replace_fingerprint(source, fingerprint, &mut catalog_budget)
        .unwrap();
    let catalog = transaction.commit(&mut catalog_budget).unwrap();

    let error = PreparedState::new(
        snapshot,
        catalog,
        DigestV1::hash_bytes(b"mismatched-yaml"),
        artifacts,
        vec![PreparedSourceBinding::new(source, fingerprint, artifact)],
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();

    assert_eq!(error.prepare_stage(), PrepareStage::IndependentReparse);
    assert!(matches!(
        error,
        PreparedStateBuildError::IndependentReparse(_)
    ));
    assert!(error.to_string().contains("prepared state validation"));
}

#[test]
fn prepared_state_rejects_source_deletion_and_orphan_output_roots() {
    let (_directory, workspace, source) = loaded_yaml_workspace();
    let snapshot = workspace.snapshot();
    let mut catalog_budget = AssetLoadBudget::default();
    let mut removal = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut catalog_budget)
        .unwrap();
    removal.remove_subtree(source, &mut catalog_budget).unwrap();
    let removed_catalog = removal.commit(&mut catalog_budget).unwrap();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let empty_artifacts = Arc::new(
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget)
            .unwrap()
            .seal_output_names()
            .unwrap()
            .finish()
            .unwrap(),
    );
    let deletion = PreparedState::new(
        snapshot.clone(),
        removed_catalog,
        DigestV1::hash_bytes(b"source-deletion"),
        empty_artifacts,
        Vec::new(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert_eq!(deletion.prepare_stage(), PrepareStage::PreparedView);
    assert!(deletion.to_string().contains("prepared state validation"));

    let (artifacts, artifact, fingerprint) = prepared_yaml_artifact_with_orphan(source);
    let mut catalog_budget = AssetLoadBudget::default();
    let mut transaction = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut catalog_budget)
        .unwrap();
    transaction
        .replace_fingerprint(source, fingerprint, &mut catalog_budget)
        .unwrap();
    let catalog = transaction.commit(&mut catalog_budget).unwrap();
    let orphan = PreparedState::new(
        snapshot,
        catalog,
        DigestV1::hash_bytes(b"orphan-output"),
        artifacts,
        vec![PreparedSourceBinding::new(source, fingerprint, artifact)],
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert_eq!(orphan.prepare_stage(), PrepareStage::PreparedView);
    assert!(orphan.to_string().contains("prepared state validation"));
}

#[test]
fn prepared_state_build_errors_own_their_prepare_stage() {
    let workspace = PreparedStateBuildError::Workspace(invalid_prepared_state("workspace"));
    let independent_reparse =
        PreparedStateBuildError::IndependentReparse(invalid_prepared_state("reparse"));
    let reference = PreparedStateBuildError::Reference(ReferenceGraphError::Invariant("reference"));

    assert_eq!(workspace.prepare_stage(), PrepareStage::PreparedView);
    assert_eq!(
        independent_reparse.prepare_stage(),
        PrepareStage::IndependentReparse
    );
    assert_eq!(reference.prepare_stage(), PrepareStage::PreparedView);
}

fn build_yaml_state_with_limits(limits: AssetLoadLimits) -> Result<u64, PreparedStateBuildError> {
    let (_directory, workspace, source) = loaded_yaml_workspace();
    let snapshot = workspace.snapshot();
    let (artifacts, artifact, fingerprint) = prepared_yaml_artifact(source);
    let mut catalog_budget = AssetLoadBudget::default();
    let mut transaction = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut catalog_budget)
        .unwrap();
    transaction
        .replace_fingerprint(source, fingerprint, &mut catalog_budget)
        .unwrap();
    let catalog = transaction.commit(&mut catalog_budget).unwrap();
    let mut budget = AssetLoadBudget::new(limits).unwrap();
    PreparedState::new(
        snapshot,
        catalog,
        DigestV1::hash_bytes(b"exact-budget"),
        artifacts,
        vec![PreparedSourceBinding::new(source, fingerprint, artifact)],
        &mut budget,
    )?;
    Ok(budget.usage().bytes)
}

#[test]
fn yaml_artifact_materialization_has_one_exactly_budgeted_backing_allocation() {
    let (_directory, _workspace, source) = loaded_yaml_workspace();
    let (artifacts, handle, _fingerprint) = prepared_yaml_artifact(source);
    let artifact = artifacts.artifact(handle).unwrap();
    let length = usize::try_from(artifact.len()).unwrap();
    let retained = arc_slice_allocation_bytes::<u8>(length).unwrap();
    let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: retained,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let (encoded, allocation_count) =
        count_allocations_at_least(length, || materialize_artifact(artifact, &mut exact_budget));
    let encoded = encoded.unwrap();
    assert_eq!(encoded.as_ref(), PREPARED_YAML.as_bytes());
    assert_eq!(exact_budget.usage().bytes, retained);
    assert_eq!(allocation_count, 1);

    let mut one_short_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: retained - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let (failure, allocation_count) = count_allocations_at_least(length, || {
        materialize_artifact(artifact, &mut one_short_budget)
    });
    assert!(matches!(
        failure,
        Err(WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));
    assert_eq!(allocation_count, 0);
}

#[test]
fn prepared_state_accepts_an_exact_byte_budget_and_rejects_one_short() {
    let required = build_yaml_state_with_limits(AssetLoadLimits::default()).unwrap();
    assert!(required > 0);
    let one_short = build_yaml_state_with_limits(AssetLoadLimits {
        max_bytes: required - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap_err();
    assert!(matches!(
        one_short,
        PreparedStateBuildError::Workspace(WorkspaceError::Budget(BudgetError::Exceeded { .. }))
            | PreparedStateBuildError::IndependentReparse(WorkspaceError::Budget(
                BudgetError::Exceeded { .. }
            ))
            | PreparedStateBuildError::Reference(ReferenceGraphError::Budget(
                BudgetError::Exceeded { .. }
            ))
    ));
    assert_eq!(
        build_yaml_state_with_limits(AssetLoadLimits {
            max_bytes: required,
            ..AssetLoadLimits::default()
        })
        .unwrap(),
        required
    );
}

#[test]
fn prepared_binary_objects_are_derived_from_the_exact_serialized_artifact() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("main.assets");
    fs::write(&path, V22_SERIALIZED_FILE).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let source = workspace
        .load_source(
            SourceOpenRequest::new(path, SourceAlias::new("main.assets").unwrap())
                .with_kind_hint(SourceKind::SerializedFile),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let snapshot = workspace.snapshot();

    let backing: Arc<[u8]> = Arc::from(V22_SERIALIZED_FILE);
    let file = SerializedFileParser::from_shared_range(
        SharedBytes::Arc(Arc::clone(&backing)),
        0..backing.len(),
    )
    .unwrap();
    let path_id = file.objects()[0].path_id();
    let mut replacement = file
        .object_handles()
        .next()
        .unwrap()
        .raw_data()
        .unwrap()
        .to_vec();
    replacement[20] = 3;
    let image = VerifiedSourceImage::verify(SourceKind::SerializedFile, backing);
    let payload = ArtifactPayload::source_backed(source, image).unwrap();
    let mut edits = SerializedFileEdits::default();
    edits
        .try_set_object_bytes(
            path_id,
            replacement.clone(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(LogicalArtifactName::new("main.assets").unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let artifact = SerializedFileWriter::prepare(
        &mut batch,
        &file,
        &edits,
        Some(SerializedFileSource::whole(&payload).unwrap()),
    )
    .unwrap();
    batch.bind_output(output, artifact).unwrap();
    let artifacts = Arc::new(batch.finish().unwrap());
    let fingerprint = SourceFingerprint::new(
        source.kind(),
        artifacts.artifact(artifact).unwrap().digest(),
    );
    let mut catalog_budget = AssetLoadBudget::default();
    let mut transaction = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut catalog_budget)
        .unwrap();
    transaction
        .replace_fingerprint(source, fingerprint, &mut catalog_budget)
        .unwrap();
    let catalog = transaction.commit(&mut catalog_budget).unwrap();
    let state = PreparedState::new(
        snapshot.clone(),
        catalog,
        DigestV1::hash_bytes(b"binary-exact-proof"),
        artifacts,
        vec![PreparedSourceBinding::new(source, fingerprint, artifact)],
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let view = PreparedView::new(state);
    let handle = WorkspaceView::objects(&view, &mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .find(|handle| handle.object().binary_path_id() == Some(path_id))
        .unwrap();
    let exact =
        WorkspaceView::read_object(&view, &handle, &mut AssetLoadBudget::default()).unwrap();
    let WorkspaceObjectValue::Binary(exact) = exact.value() else {
        panic!("SerializedFile object must remain binary");
    };
    assert_eq!(exact.raw_data(), replacement);
    assert!(matches!(
        exact.payload_provenance(),
        ObjectPayloadProvenance::TypedReplacement | ObjectPayloadProvenance::RawReplacement
    ));

    let baseline_handle = handle.clone().with_revision(snapshot.revision());
    let baseline =
        WorkspaceView::read_object(&snapshot, &baseline_handle, &mut AssetLoadBudget::default())
            .unwrap();
    let WorkspaceObjectValue::Binary(baseline) = baseline.value() else {
        panic!("baseline object must be binary");
    };
    assert_ne!(baseline.raw_data(), replacement);
}
