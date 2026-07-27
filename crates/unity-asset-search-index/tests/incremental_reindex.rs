use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationValue, PrepareOptions,
    PublicationTarget, SourceExpectation, SourceOpenRequest, WorkspaceOptions,
};
use unity_asset::{
    AssetLoadBudget, ChangeSet, DigestV1, FieldPath, ObjectAddress, SourceAlias, SourceFingerprint,
    SourceId, SourceKind, SourceLocator, TransactionId, UnityClass, UnityValue,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};
use unity_asset_search_index::{
    ApiErrorCode, GenerationStamp, IndexPaths, ReferenceRequest, ReferencesResponse,
    ReindexDisposition, ReindexIntent, ReindexReceipt, SearchIndex, SearchRequest, SearchResponse,
};

const OWNER_ALIAS: &str = "Assets/owner.prefab";
const TARGET_ALIAS: &str = "Assets/target.prefab";
const TARGET_GUID: &str = "0123456789abcdef0123456789abcdef";
const REPLACEMENT_TARGET_GUID: &str = "fedcba9876543210fedcba9876543210";

const OWNER_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: LegacyBeacon
  m_Target: {fileID: 100, guid: 0123456789abcdef0123456789abcdef, type: 3}
"#;

const TARGET_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: Before
"#;

const CLOSURE_OWNER_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: ClosureBeacon
  m_Target: {fileID: 101, guid: fedcba9876543210fedcba9876543210, type: 3}
"#;

const CLOSURE_TARGET_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: ClosureTarget
"#;

struct Fixture {
    _temporary: TempDir,
    project_root: PathBuf,
    paths: IndexPaths,
    index: SearchIndex,
    workspace: AssetWorkspace,
    owner_source: SourceId,
    target_source: SourceId,
    baseline: GenerationStamp,
}

fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let project_root = temporary.path().join("project");
    let assets = project_root.join("Assets");
    fs::create_dir_all(&assets).unwrap();
    let owner_path = assets.join("owner.prefab");
    let target_path = assets.join("target.prefab");
    fs::write(&owner_path, OWNER_YAML).unwrap();
    fs::write(&target_path, TARGET_YAML).unwrap();
    fs::write(
        assets.join("target.prefab.meta"),
        format!("fileFormatVersion: 2\nguid: {TARGET_GUID}\n"),
    )
    .unwrap();

    let paths = IndexPaths::for_project(
        project_root.clone(),
        Some(temporary.path().join("index")),
        None,
    )
    .unwrap();
    let index =
        SearchIndex::open_or_create(paths.clone(), &mut AssetLoadBudget::default()).unwrap();
    let baseline_receipt = index
        .reindex(ReindexIntent::full(), &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(baseline_receipt.disposition, ReindexDisposition::Applied);
    assert!(baseline_receipt.evidence.analysis.assets_visited > 0);
    assert!(baseline_receipt.evidence.analysis.assets_analyzed > 0);
    assert!(baseline_receipt.evidence.analysis.source_opens > 0);
    assert!(baseline_receipt.evidence.analysis.source_bytes_read > 0);
    let baseline = assert_active_receipt(&index, &baseline_receipt);

    let mut workspace =
        AssetWorkspace::with_workspace_id(baseline.workspace, WorkspaceOptions::lenient()).unwrap();
    let owner_source = workspace
        .load_source(
            SourceOpenRequest::new(
                &owner_path,
                SourceAlias::new(OWNER_ALIAS.to_owned()).unwrap(),
            )
            .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let target_source = workspace
        .load_source(
            SourceOpenRequest::new(
                &target_path,
                SourceAlias::new(TARGET_ALIAS.to_owned()).unwrap(),
            )
            .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(workspace.revision(), baseline.actual_revision);

    assert_eq!(
        search_paths_at_generation(&index, &baseline, "Before"),
        vec![TARGET_ALIAS.to_owned()]
    );
    assert_eq!(
        incoming_reference_paths_at_generation(&index, &baseline, TARGET_GUID, 100),
        vec![OWNER_ALIAS.to_owned()]
    );

    Fixture {
        _temporary: temporary,
        project_root,
        paths,
        index,
        workspace,
        owner_source,
        target_source,
        baseline,
    }
}

fn target_address() -> ObjectAddress {
    ObjectAddress::yaml(SourceLocator::path(TARGET_ALIAS).unwrap(), "100").unwrap()
}

fn name_path() -> FieldPath {
    FieldPath::root().push_field("m_Name").unwrap()
}

fn name_guard(value: &str) -> FieldGuard {
    let class = UnityClass::new(1, "GameObject".to_owned(), "100".to_owned());
    let path = name_path();
    let value = UnityValue::String(value.to_owned());
    let mut budget = AssetLoadBudget::default();
    FieldGuard::new(
        yaml_field_schema_digest(&class, &path, &value, &mut budget).unwrap(),
        semantic_value_digest(&value, &mut budget).unwrap(),
    )
}

fn commit_target_name(fixture: &mut Fixture, replacement: &str) -> ChangeSet {
    let plan = MutationPlan::new(
        fixture.workspace.workspace_id(),
        fixture.workspace.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(TARGET_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, TARGET_YAML.as_bytes()),
        )],
        Vec::new(),
        vec![GenericMutation::FieldReplace {
            target: target_address(),
            path: name_path(),
            guard: name_guard("Before"),
            replacement: MutationValue::string(replacement).unwrap(),
        }],
    )
    .unwrap();
    let prepared = fixture
        .workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let report = fixture
        .workspace
        .commit(
            prepared,
            PublicationTarget::in_place(&fixture.project_root).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(report.committed_revision(), fixture.workspace.revision());
    assert_eq!(report.changes().transaction(), report.transaction());
    report.changes().clone()
}

fn active_generation(index: &SearchIndex) -> GenerationStamp {
    let status = index.status().unwrap();
    assert!(!status.indexing);
    assert!(status.progress.is_none());
    status
        .generation
        .active
        .expect("the index has an active generation")
}

fn assert_active_receipt(index: &SearchIndex, receipt: &ReindexReceipt) -> GenerationStamp {
    let generation = receipt
        .generation
        .clone()
        .expect("a successful reindex returns its active generation");
    let status = index.status().unwrap();
    assert_eq!(status.generation.active.as_ref(), Some(&generation));
    assert_eq!(generation.actual_revision, generation.desired_revision);
    assert!(!generation.stale);
    assert_eq!(status.generation.building_revision, None);
    assert!(!status.indexing);
    assert!(status.progress.is_none());
    generation
}

fn search_response_at_generation(
    index: &SearchIndex,
    generation: &GenerationStamp,
    query: &str,
) -> SearchResponse {
    let status_before = serde_json::to_value(index.status().unwrap()).unwrap();
    let response = index.search(SearchRequest::new(query, 20)).unwrap();
    assert_eq!(&response.generation, generation);
    let status_after = serde_json::to_value(index.status().unwrap()).unwrap();
    assert_eq!(
        status_after, status_before,
        "search must not mutate generation or runtime status"
    );
    response
}

fn search_paths_at_generation(
    index: &SearchIndex,
    generation: &GenerationStamp,
    query: &str,
) -> Vec<String> {
    search_response_at_generation(index, generation, query)
        .hits
        .into_iter()
        .map(|hit| hit.path)
        .collect()
}

fn reference_response_at_generation(
    index: &SearchIndex,
    generation: &GenerationStamp,
    guid: &str,
    file_id: i64,
) -> ReferencesResponse {
    let status_before = serde_json::to_value(index.status().unwrap()).unwrap();
    let response = index
        .references(
            ReferenceRequest::incoming_guid(guid, Some(file_id), 20),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(&response.generation, generation);
    let status_after = serde_json::to_value(index.status().unwrap()).unwrap();
    assert_eq!(
        status_after, status_before,
        "reference lookup must not mutate generation or runtime status"
    );
    response
}

fn incoming_reference_paths_at_generation(
    index: &SearchIndex,
    generation: &GenerationStamp,
    guid: &str,
    file_id: i64,
) -> Vec<String> {
    reference_response_at_generation(index, generation, guid, file_id)
        .hits
        .into_iter()
        .map(|hit| hit.source_path)
        .collect()
}

#[test]
fn committed_change_set_is_idempotent_conflict_checked_and_persistent() {
    let mut fixture = fixture();
    let changes = commit_target_name(&mut fixture, "After");
    let target_revision = fixture.workspace.revision();

    let applied = fixture
        .index
        .reindex_workspace(
            changes.clone(),
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(applied.disposition, ReindexDisposition::Applied);
    assert_eq!(applied.transaction, Some(changes.transaction()));
    assert_eq!(applied.target_revision, Some(target_revision));
    assert!(applied.evidence.analysis.assets_visited > 0);
    assert!(applied.evidence.analysis.assets_analyzed > 0);
    let applied_generation = assert_active_receipt(&fixture.index, &applied);
    assert_eq!(applied_generation.actual_revision, target_revision);
    assert_eq!(applied_generation.desired_revision, target_revision);
    assert!(!applied_generation.stale);
    assert_eq!(
        search_paths_at_generation(&fixture.index, &applied_generation, "Before"),
        Vec::<String>::new()
    );
    assert_eq!(
        search_paths_at_generation(&fixture.index, &applied_generation, "After"),
        vec![TARGET_ALIAS.to_owned()]
    );
    assert_eq!(
        incoming_reference_paths_at_generation(
            &fixture.index,
            &applied_generation,
            TARGET_GUID,
            100,
        ),
        vec![OWNER_ALIAS.to_owned()]
    );

    let before_duplicate = fixture.index.status().unwrap();
    let duplicate = fixture
        .index
        .reindex_workspace(
            changes.clone(),
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(duplicate.disposition, ReindexDisposition::AlreadyApplied);
    assert_eq!(duplicate.generation, Some(applied_generation.clone()));
    let duplicate_generation = assert_active_receipt(&fixture.index, &duplicate);
    assert_eq!(duplicate_generation, applied_generation);
    let after_duplicate = fixture.index.status().unwrap();
    assert_eq!(
        after_duplicate.generation.active,
        before_duplicate.generation.active
    );
    assert_eq!(
        after_duplicate.indexed_search_documents,
        before_duplicate.indexed_search_documents
    );
    assert_eq!(
        after_duplicate.indexed_reference_facts,
        before_duplicate.indexed_reference_facts
    );
    assert_eq!(
        search_paths_at_generation(&fixture.index, &duplicate_generation, "After"),
        vec![TARGET_ALIAS.to_owned()]
    );
    assert_eq!(
        incoming_reference_paths_at_generation(
            &fixture.index,
            &duplicate_generation,
            TARGET_GUID,
            100,
        ),
        vec![OWNER_ALIAS.to_owned()]
    );

    let conflicting = ChangeSet::new(
        changes.transaction(),
        changes.workspace(),
        changes.from_revision(),
        changes.to_revision(),
        vec![fixture.owner_source],
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    let expected_conflict_message = format!(
        "transaction {} conflicts with its persisted ChangeSet receipt",
        changes.transaction()
    );
    let conflict_error = fixture
        .index
        .reindex_workspace(
            conflicting.clone(),
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert_eq!(conflict_error.code(), ApiErrorCode::InvalidRequest);
    assert_eq!(conflict_error.to_string(), expected_conflict_message);
    assert_eq!(
        fixture.index.status().unwrap().generation.active,
        Some(applied_generation.clone())
    );

    let expected_status = fixture.index.status().unwrap();
    let expected_search =
        search_response_at_generation(&fixture.index, &applied_generation, "After");
    let expected_references =
        reference_response_at_generation(&fixture.index, &applied_generation, TARGET_GUID, 100);
    let paths = fixture.paths.clone();
    drop(fixture.index);

    let reopened = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
    let reopened_status = reopened.status().unwrap();
    assert_eq!(
        reopened_status.generation.active,
        expected_status.generation.active
    );
    assert_eq!(
        reopened_status.indexed_assets,
        expected_status.indexed_assets
    );
    assert_eq!(
        reopened_status.indexed_search_documents,
        expected_status.indexed_search_documents
    );
    assert_eq!(
        reopened_status.indexed_reference_facts,
        expected_status.indexed_reference_facts
    );
    let reopened_generation = active_generation(&reopened);
    assert_eq!(reopened_generation, applied_generation);

    let reopened_search = search_response_at_generation(&reopened, &reopened_generation, "After");
    assert_eq!(reopened_search.generation, expected_search.generation);
    assert_eq!(
        serde_json::to_value((&reopened_search.match_count, &reopened_search.hits)).unwrap(),
        serde_json::to_value((&expected_search.match_count, &expected_search.hits)).unwrap()
    );
    let reopened_references =
        reference_response_at_generation(&reopened, &reopened_generation, TARGET_GUID, 100);
    assert_eq!(
        reopened_references.generation,
        expected_references.generation
    );
    assert_eq!(
        serde_json::to_value((&reopened_references.coverage, &reopened_references.hits)).unwrap(),
        serde_json::to_value((&expected_references.coverage, &expected_references.hits)).unwrap()
    );

    let reopened_conflict_error = reopened
        .reindex_workspace(
            conflicting,
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert_eq!(reopened_conflict_error.code(), ApiErrorCode::InvalidRequest);
    assert_eq!(
        reopened_conflict_error.to_string(),
        expected_conflict_message
    );
    assert_eq!(active_generation(&reopened), applied_generation);

    let reopened_duplicate = reopened
        .reindex_workspace(
            changes,
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(
        reopened_duplicate.disposition,
        ReindexDisposition::AlreadyApplied
    );
    assert_eq!(
        reopened_duplicate.generation,
        Some(applied_generation.clone())
    );
    let reopened_duplicate_generation = assert_active_receipt(&reopened, &reopened_duplicate);
    assert_eq!(reopened_duplicate_generation, applied_generation);
    assert_eq!(
        search_paths_at_generation(&reopened, &reopened_duplicate_generation, "After"),
        vec![TARGET_ALIAS.to_owned()]
    );
    assert_eq!(
        incoming_reference_paths_at_generation(
            &reopened,
            &reopened_duplicate_generation,
            TARGET_GUID,
            100,
        ),
        vec![OWNER_ALIAS.to_owned()]
    );
}

#[test]
fn filesystem_reconciliation_preserves_lagging_change_set_receipts_across_reopen() {
    let mut fixture = fixture();
    let changes = commit_target_name(&mut fixture, "After");

    let applied = fixture
        .index
        .reindex_workspace(
            changes.clone(),
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(applied.disposition, ReindexDisposition::Applied);

    fs::write(
        fixture.project_root.join(TARGET_ALIAS),
        TARGET_YAML.replace("Before", "FilesystemOnly"),
    )
    .unwrap();
    let reconciled = fixture
        .index
        .reindex(ReindexIntent::reconcile(), &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(reconciled.disposition, ReindexDisposition::Applied);
    let reconciled_generation = assert_active_receipt(&fixture.index, &reconciled);
    assert_ne!(
        reconciled_generation.actual_revision,
        changes.to_revision(),
        "filesystem reconciliation must advance beyond the receipt's target revision"
    );

    let paths = fixture.paths.clone();
    drop(fixture.index);
    let reopened = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
    assert_eq!(active_generation(&reopened), reconciled_generation);

    let duplicate = reopened
        .reindex_workspace(
            changes.clone(),
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(duplicate.disposition, ReindexDisposition::AlreadyApplied);
    assert_eq!(duplicate.transaction, Some(changes.transaction()));
    assert_eq!(duplicate.target_revision, Some(changes.to_revision()));
    assert_eq!(duplicate.generation, Some(reconciled_generation));
}

#[test]
fn failed_delivery_keeps_the_stale_generation_queryable_until_reconciliation() {
    let mut fixture = fixture();
    let stale_view = fixture.workspace.snapshot();
    let changes = commit_target_name(&mut fixture, "After");
    let target_revision = changes.to_revision();

    let failed =
        fixture
            .index
            .reindex_workspace(changes, &stale_view, &mut AssetLoadBudget::default());
    assert_eq!(failed.unwrap_err().code(), ApiErrorCode::RevisionMismatch);

    let stale_status = fixture.index.status().unwrap();
    let stale_generation = stale_status.generation.active.unwrap();
    assert_eq!(stale_generation.generation, fixture.baseline.generation);
    assert_eq!(
        stale_generation.actual_revision,
        fixture.baseline.actual_revision
    );
    assert_eq!(stale_generation.desired_revision, target_revision);
    assert!(stale_generation.stale);
    assert_eq!(
        stale_status
            .generation
            .last_failure
            .as_ref()
            .unwrap()
            .desired_revision,
        Some(target_revision)
    );
    let stale_search = search_response_at_generation(&fixture.index, &stale_generation, "Before");
    assert_eq!(
        stale_search
            .hits
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        vec![TARGET_ALIAS]
    );
    assert_eq!(
        search_paths_at_generation(&fixture.index, &stale_generation, "After"),
        Vec::<String>::new()
    );
    assert_eq!(
        incoming_reference_paths_at_generation(&fixture.index, &stale_generation, TARGET_GUID, 100,),
        vec![OWNER_ALIAS.to_owned()]
    );

    let reconciled = fixture
        .index
        .reindex(ReindexIntent::reconcile(), &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(reconciled.disposition, ReindexDisposition::Applied);
    assert!(reconciled.evidence.dependency_closure_assets >= 1);
    assert!(reconciled.evidence.analysis.assets_visited > 0);
    assert!(reconciled.evidence.analysis.assets_analyzed > 0);
    assert!(reconciled.evidence.analysis.source_opens > 0);
    assert!(reconciled.evidence.analysis.source_bytes_read > 0);
    let reconciled_generation = assert_active_receipt(&fixture.index, &reconciled);
    assert_eq!(reconciled_generation.actual_revision, target_revision);
    assert_eq!(reconciled_generation.desired_revision, target_revision);
    assert!(!reconciled_generation.stale);
    assert_ne!(
        reconciled_generation.generation,
        fixture.baseline.generation
    );
    assert!(
        fixture
            .index
            .status()
            .unwrap()
            .generation
            .last_failure
            .is_none()
    );
    assert_eq!(
        search_paths_at_generation(&fixture.index, &reconciled_generation, "Before"),
        Vec::<String>::new()
    );
    assert_eq!(
        search_paths_at_generation(&fixture.index, &reconciled_generation, "After"),
        vec![TARGET_ALIAS.to_owned()]
    );
    assert_eq!(
        incoming_reference_paths_at_generation(
            &fixture.index,
            &reconciled_generation,
            TARGET_GUID,
            100,
        ),
        vec![OWNER_ALIAS.to_owned()]
    );
}

#[test]
fn reported_target_change_reanalyzes_its_modified_dependency_owner_once() {
    let mut fixture = fixture();
    let from_revision = fixture.workspace.revision();

    fixture
        .workspace
        .unload_source(fixture.owner_source, &mut AssetLoadBudget::default())
        .unwrap();
    fixture
        .workspace
        .unload_source(fixture.target_source, &mut AssetLoadBudget::default())
        .unwrap();

    let owner_path = fixture.project_root.join(OWNER_ALIAS);
    let target_path = fixture.project_root.join(TARGET_ALIAS);
    fs::write(&owner_path, CLOSURE_OWNER_YAML).unwrap();
    fs::write(&target_path, CLOSURE_TARGET_YAML).unwrap();

    let reloaded_owner = fixture
        .workspace
        .load_source(
            SourceOpenRequest::new(
                &owner_path,
                SourceAlias::new(OWNER_ALIAS.to_owned()).unwrap(),
            )
            .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let reloaded_target = fixture
        .workspace
        .load_source(
            SourceOpenRequest::new(
                &target_path,
                SourceAlias::new(TARGET_ALIAS.to_owned()).unwrap(),
            )
            .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(reloaded_owner, fixture.owner_source);
    assert_eq!(reloaded_target, fixture.target_source);

    let target_revision = fixture.workspace.revision();
    let changes = ChangeSet::new(
        TransactionId::new(DigestV1::hash_bytes(b"dependency-closure-transaction")),
        fixture.workspace.workspace_id(),
        from_revision,
        target_revision,
        vec![reloaded_target],
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(changes.changed_sources(), &[reloaded_target]);
    assert!(!changes.changed_sources().contains(&reloaded_owner));

    let receipt = fixture
        .index
        .reindex_workspace(
            changes.clone(),
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(receipt.disposition, ReindexDisposition::Applied);
    assert_eq!(receipt.transaction, Some(changes.transaction()));
    assert_eq!(receipt.target_revision, Some(target_revision));
    assert!(!receipt.evidence.forced_full_analysis);
    assert_eq!(
        receipt.evidence.dependency_closure_assets, 1,
        "only the owner should be added by dependency closure"
    );
    assert_eq!(
        receipt.evidence.analysis.assets_visited, 2,
        "the reported target and dependency owner must each be visited once"
    );
    assert_eq!(
        receipt.evidence.analysis.assets_analyzed, 2,
        "the reported target and modified owner must each be analyzed once"
    );
    assert_eq!(receipt.evidence.analysis.yaml_documents, 2);
    assert_eq!(receipt.evidence.analysis.references_emitted, 1);

    let generation = assert_active_receipt(&fixture.index, &receipt);
    assert_eq!(generation.actual_revision, target_revision);
    assert_eq!(
        search_paths_at_generation(&fixture.index, &generation, "ClosureBeacon"),
        vec![OWNER_ALIAS.to_owned()]
    );
    assert_eq!(
        search_paths_at_generation(&fixture.index, &generation, "LegacyBeacon"),
        Vec::<String>::new()
    );
    assert_eq!(
        search_paths_at_generation(&fixture.index, &generation, "ClosureTarget"),
        vec![TARGET_ALIAS.to_owned()]
    );
    assert_eq!(
        incoming_reference_paths_at_generation(&fixture.index, &generation, TARGET_GUID, 100,),
        Vec::<String>::new()
    );
    assert_eq!(
        incoming_reference_paths_at_generation(
            &fixture.index,
            &generation,
            REPLACEMENT_TARGET_GUID,
            101,
        ),
        vec![OWNER_ALIAS.to_owned()]
    );
}

#[test]
fn deleted_source_forces_complete_dependency_reanalysis() {
    let mut fixture = fixture();
    let from_revision = fixture.workspace.revision();
    fixture
        .workspace
        .unload_source(fixture.target_source, &mut AssetLoadBudget::default())
        .unwrap();
    let target_revision = fixture.workspace.revision();
    let changes = ChangeSet::new(
        TransactionId::new(DigestV1::hash_bytes(b"delete-target-transaction")),
        fixture.workspace.workspace_id(),
        from_revision,
        target_revision,
        vec![fixture.target_source],
        Vec::new(),
        Vec::new(),
    )
    .unwrap();

    let receipt = fixture
        .index
        .reindex_workspace(
            changes,
            &fixture.workspace.snapshot(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(receipt.disposition, ReindexDisposition::Applied);
    assert!(receipt.evidence.forced_full_analysis);
    assert!(receipt.evidence.dependency_closure_assets >= 1);
    assert!(receipt.evidence.analysis.assets_visited > 0);
    assert!(receipt.evidence.analysis.assets_analyzed > 0);
    let generation = assert_active_receipt(&fixture.index, &receipt);
    assert_eq!(generation.actual_revision, target_revision);
    assert_eq!(generation.desired_revision, target_revision);
    assert!(!generation.stale);
    assert_eq!(fixture.index.status().unwrap().indexed_assets, 1);
    assert_eq!(
        search_paths_at_generation(&fixture.index, &generation, "Before"),
        Vec::<String>::new()
    );
    assert_eq!(
        incoming_reference_paths_at_generation(&fixture.index, &generation, TARGET_GUID, 100),
        vec![OWNER_ALIAS.to_owned()]
    );
}
