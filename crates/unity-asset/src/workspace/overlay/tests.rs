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
    SourceFingerprint, SourceKind, UnityValue, VerifiedSourceImage,
};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload, LogicalArtifactName,
};
use unity_asset_write::object::{
    SerializedObjectEncoder, UnsafeRawObjectAcknowledgement, UnsafeRawObjectReplacement,
};
use unity_asset_write::serialized_file::{
    SerializedFileEdits, SerializedFileSource, SerializedFileWriter,
};

use crate::reference::{RawReferenceTarget, ReferenceGraphBuildOptions, ReferenceResolution};

use super::*;
use crate::workspace::source_catalog::PhysicalDomainChange;
use crate::workspace::{
    AssetWorkspace, SourceOpenRequest, WorkspaceInspector, WorkspaceSourceFormatInspection,
};

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
        .rewrite_physical_domains_from_changes(
            &[PhysicalDomainChange::new(source, fingerprint)],
            &mut budget,
        )
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

    let inspected = WorkspaceInspector::new(&view)
        .source(source, &mut AssetLoadBudget::default())
        .unwrap();
    let WorkspaceLookup::Resolved(inspected) = inspected else {
        panic!("prepared source inspection must resolve");
    };
    assert!(matches!(
        inspected.format(),
        WorkspaceSourceFormatInspection::Yaml { document_count: 2 }
    ));

    let objects = WorkspaceView::objects(&view, &mut AssetLoadBudget::default()).unwrap();
    assert!(
        objects
            .iter()
            .all(|handle| handle.revision() == view.revision())
    );
    let first = objects
        .iter()
        .find(|handle| handle.object().yaml_file_id() == Some("1".parse().unwrap()))
        .unwrap();
    let second = objects
        .iter()
        .find(|handle| handle.object().yaml_file_id() == Some("2".parse().unwrap()))
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
        .find(|fact| fact.source().object().yaml_file_id() == Some("1".parse().unwrap()))
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
        .find(|fact| fact.source().object().yaml_file_id() == Some("1".parse().unwrap()))
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
    assert_eq!(
        WorkspaceView::source_length(&view, source).unwrap(),
        u64::try_from(PREPARED_YAML.len()).unwrap()
    );
    assert_eq!(
        WorkspaceView::source_length(&snapshot, source).unwrap(),
        u64::try_from(BASE_YAML.len()).unwrap()
    );
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
        .rewrite_physical_domains_from_changes(
            &[PhysicalDomainChange::new(source, fingerprint)],
            &mut catalog_budget,
        )
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
        .rewrite_physical_domains_from_changes(
            &[PhysicalDomainChange::new(source, fingerprint)],
            &mut catalog_budget,
        )
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
        .rewrite_physical_domains_from_changes(
            &[PhysicalDomainChange::new(source, fingerprint)],
            &mut catalog_budget,
        )
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
fn prepared_yaml_proof_is_reused_by_workspace_without_reparsing() {
    let (_directory, workspace, source) = loaded_yaml_workspace();
    let snapshot = workspace.snapshot();
    let (artifacts, handle, fingerprint) = prepared_yaml_artifact(source);
    let proof_document = match artifacts
        .artifact(handle)
        .unwrap()
        .prove_source_compatibility(source, fingerprint)
        .unwrap()
        .format()
    {
        PreparedArtifactFormatProof::Yaml(proof) => Arc::clone(proof.document()),
        _ => panic!("prepared fixture must carry a YAML proof"),
    };

    let mut budget = AssetLoadBudget::default();
    let mut transaction = snapshot
        .state()
        .catalog()
        .begin_transaction(&mut budget)
        .unwrap();
    transaction
        .rewrite_physical_domains_from_changes(
            &[PhysicalDomainChange::new(source, fingerprint)],
            &mut budget,
        )
        .unwrap();
    let catalog = transaction.commit(&mut budget).unwrap();
    let state = PreparedState::new(
        snapshot,
        catalog,
        DigestV1::hash_bytes(b"prepared-yaml-proof-reuse"),
        artifacts,
        vec![PreparedSourceBinding::new(source, fingerprint, handle)],
        &mut budget,
    )
    .unwrap();
    let bound_document = state
        .core
        .source_binding(source)
        .and_then(ProvenPreparedSourceBinding::yaml_document)
        .expect("prepared YAML source must retain its exact document proof");
    assert!(Arc::ptr_eq(&proof_document, bound_document));
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

    let baseline_format = snapshot.state().store().get(source).unwrap().format();
    let mut clone_measurement = AssetLoadBudget::default();
    let measured_clone = baseline_format
        .try_clone_with_budget(&mut clone_measurement)
        .unwrap();
    let clone_bytes = clone_measurement.usage().bytes;
    assert!(clone_bytes > 0);
    assert_eq!(&measured_clone, baseline_format);

    let mut exact_clone_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: clone_bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert_eq!(
        baseline_format
            .try_clone_with_budget(&mut exact_clone_budget)
            .unwrap(),
        measured_clone
    );
    assert_eq!(exact_clone_budget.usage().bytes, clone_bytes);

    let mut one_short_clone_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: clone_bytes - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let (clone_failure, allocation_count) = count_allocations_at_least(1, || {
        baseline_format.try_clone_with_budget(&mut one_short_clone_budget)
    });
    assert!(matches!(
        clone_failure,
        Err(WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));
    assert_eq!(allocation_count, 0);

    let backing: Arc<[u8]> = Arc::from(V22_SERIALIZED_FILE);
    let file = SerializedFileParser::from_shared_range(
        SharedBytes::from_arc(Arc::clone(&backing)),
        0..backing.len(),
    )
    .unwrap();
    let path_id = file.objects()[0].path_id();
    let original = file.object_handles().next().unwrap().raw_data().unwrap();
    let original_digest = DigestV1::hash_bytes(original);
    let mut replacement = original.to_vec();
    replacement[20] = 3;
    let image = VerifiedSourceImage::verify(SourceKind::SerializedFile, backing);
    let payload = ArtifactPayload::source_backed(source, image).unwrap();
    let mut edit_budget = AssetLoadBudget::default();
    let encoded = SerializedObjectEncoder::new(&file, path_id)
        .unwrap()
        .encode_unsafe_raw(
            UnsafeRawObjectReplacement::new(
                original_digest,
                replacement.clone(),
                UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
            ),
            &mut edit_budget,
        )
        .unwrap();
    let mut edits = SerializedFileEdits::default();
    edits
        .try_insert_encoded_object(encoded, &mut edit_budget)
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
        .rewrite_physical_domains_from_changes(
            &[PhysicalDomainChange::new(source, fingerprint)],
            &mut catalog_budget,
        )
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
    let inspected = WorkspaceInspector::new(&view)
        .source(source, &mut AssetLoadBudget::default())
        .unwrap();
    let WorkspaceLookup::Resolved(inspected) = inspected else {
        panic!("prepared SerializedFile inspection must resolve");
    };
    assert_eq!(inspected.format(), &measured_clone);
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

    let prepared_descriptor = crate::workspace::object_descriptor_at_in_source(
        &view,
        source,
        0,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let baseline_descriptor = crate::workspace::object_descriptor_at_in_source(
        &snapshot,
        source,
        0,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let mut exact_measurement = AssetLoadBudget::default();
    crate::workspace::read_object_at_in_source(&view, &prepared_descriptor, &mut exact_measurement)
        .unwrap();
    let exact_usage = exact_measurement.usage();
    let mut baseline_measurement = AssetLoadBudget::default();
    crate::workspace::read_object_at_in_source(
        &snapshot,
        &baseline_descriptor,
        &mut baseline_measurement,
    )
    .unwrap();
    assert!(exact_usage.bytes < baseline_measurement.usage().bytes);

    let tight_limits = AssetLoadLimits {
        max_entries: exact_usage.entries.max(1),
        max_bytes: exact_usage.bytes.max(1),
        ..AssetLoadLimits::default()
    };
    crate::workspace::read_object_at_in_source(
        &view,
        &prepared_descriptor,
        &mut AssetLoadBudget::new(tight_limits).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        crate::workspace::read_object_at_in_source(
            &snapshot,
            &baseline_descriptor,
            &mut AssetLoadBudget::new(tight_limits).unwrap(),
        ),
        Err(WorkspaceError::Budget(_))
    ));

    let passthrough_descriptor = crate::workspace::object_descriptor_at_in_source(
        &view,
        source,
        1,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let baseline_passthrough_descriptor = crate::workspace::object_descriptor_at_in_source(
        &snapshot,
        source,
        1,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let mut passthrough_measurement = AssetLoadBudget::default();
    let passthrough = crate::workspace::read_object_at_in_source(
        &view,
        &passthrough_descriptor,
        &mut passthrough_measurement,
    )
    .unwrap();
    let WorkspaceObjectValue::Binary(passthrough) = passthrough.value() else {
        panic!("SerializedFile passthrough object must remain binary");
    };
    assert_eq!(
        passthrough.raw_data(),
        file.object_handles().nth(1).unwrap().raw_data().unwrap()
    );
    let passthrough_usage = passthrough_measurement.usage();
    assert!(passthrough_usage.entries > 1 && passthrough_usage.bytes > 1);
    let mut baseline_passthrough_measurement = AssetLoadBudget::default();
    crate::workspace::read_object_at_in_source(
        &snapshot,
        &baseline_passthrough_descriptor,
        &mut baseline_passthrough_measurement,
    )
    .unwrap();
    assert_eq!(
        passthrough_usage,
        baseline_passthrough_measurement.usage(),
        "passthrough must not charge an unsuccessful Exact projection"
    );

    let passthrough_limits = AssetLoadLimits {
        max_entries: passthrough_usage.entries,
        max_bytes: passthrough_usage.bytes,
        ..AssetLoadLimits::default()
    };
    crate::workspace::read_object_at_in_source(
        &view,
        &passthrough_descriptor,
        &mut AssetLoadBudget::new(passthrough_limits).unwrap(),
    )
    .unwrap();
    for (dimension, limits) in [
        (
            "entries",
            AssetLoadLimits {
                max_entries: passthrough_usage.entries - 1,
                ..passthrough_limits
            },
        ),
        (
            "bytes",
            AssetLoadLimits {
                max_bytes: passthrough_usage.bytes - 1,
                ..passthrough_limits
            },
        ),
    ] {
        let result = crate::workspace::read_object_at_in_source(
            &view,
            &passthrough_descriptor,
            &mut AssetLoadBudget::new(limits).unwrap(),
        );
        assert!(
            matches!(result, Err(WorkspaceError::Budget(_))),
            "passthrough {dimension} one-short boundary unexpectedly succeeded: usage={passthrough_usage:?}, result={result:?}"
        );
    }

    let baseline_handle = handle.clone().with_revision(snapshot.revision());
    let baseline =
        WorkspaceView::read_object(&snapshot, &baseline_handle, &mut AssetLoadBudget::default())
            .unwrap();
    let WorkspaceObjectValue::Binary(baseline) = baseline.value() else {
        panic!("baseline object must be binary");
    };
    assert_ne!(baseline.raw_data(), replacement);
}
