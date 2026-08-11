use std::fs;
use std::io::Read;
use std::path::PathBuf;

use unity_asset::extraction::{
    ExistingOutputPolicy, ExtractionExecutionLimits, ExtractionExecutionOptions,
    ExtractionExecutor, ExtractionFailurePolicy, ExtractionPlan, ExtractionPlanner,
    ExtractionReport, ExtractionRepresentationPolicy, ExtractionRequest, ExtractionRunOptions,
};
use unity_asset::schema::{
    AudioClipResourceRecipe, HierarchyDestinationV1, HierarchyIntentV1, HierarchyPlacementV1,
    HierarchyRecipe, SchemaRecipePlanner,
};
use unity_asset::workspace::{
    AssetWorkspace, MutationPlan, MutationPlanBuilder, MutationValue, PlanPayload, PrepareOptions,
    PublicationTarget, RecoveryOutcome, ReferenceTarget, SourceAdmissionBatch,
    SourceAdmissionOperation, SourceAdmissionPolicy, SourceCompanionRequest, SourceLocationKind,
    SourceOpenRequest, WorkspaceInspector, WorkspaceLookup, WorkspaceOptions, WorkspaceView,
    workspace_capabilities,
};
use unity_asset::{
    AssetLoadBudget, FieldPath, ObjectAddress, SourceAlias, SourceKind, SourceLocator, UnityValue,
};
use unity_asset_search_index::{IndexPaths, SearchIndex, SearchRequest};
use unity_asset_search_protocol::{ReferenceRequest, ReindexDisposition, ValidateContract};

mod common;

const SOURCE_ALIAS: &str = "Assets/agent-native.asset";
const REPLACEMENT_PAYLOAD: &[u8] = b"RIFF\x26\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x40\x1f\x00\x00\x40\x1f\x00\x00\x01\x00\x08\x00data\x02\x00\x00\x00\x80\x80";
const SOURCE: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: Before
--- !u!21 &2
Material:
  m_Name: Owner
  m_Texture: {fileID: 0}
--- !u!28 &3
Texture2D:
  m_Name: Target
--- !u!83 &4
AudioClip:
  m_Name: Clip
  m_CompressionFormat: 0
  m_SubsoundIndex: 0
  m_StreamData: {path: archive:/CAB-old/CAB-old.resS, offset: 7, size: 4}
--- !u!4 &10
Transform:
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 11}
--- !u!4 &11
Transform:
  m_Father: {fileID: 10}
  m_Children: []
--- !u!4 &12
Transform:
  m_Father: {fileID: 0}
  m_Children: []
"#;

fn address(anchor: &str) -> ObjectAddress {
    ObjectAddress::yaml(
        SourceLocator::path(SOURCE_ALIAS).unwrap(),
        anchor.parse().unwrap(),
    )
    .unwrap()
}

fn field(name: &str) -> FieldPath {
    FieldPath::root().push_field(name).unwrap()
}

fn value_at(view: &impl WorkspaceView, target: &ObjectAddress, path: &FieldPath) -> UnityValue {
    let WorkspaceLookup::Resolved(handle) = view
        .resolve_object(target, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("fixture object must resolve");
    };
    view.read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap()
        .class()
        .value_at_path(path)
        .unwrap()
        .clone()
}

fn admit_strict(workspace: &mut AssetWorkspace, requests: Vec<SourceOpenRequest>) {
    let operation_count = requests.len();
    let mut budget = AssetLoadBudget::default();
    let mut batch = SourceAdmissionBatch::with_capacity(operation_count, &mut budget).unwrap();
    for request in requests {
        batch
            .try_push(SourceAdmissionOperation::LoadPath(request), &mut budget)
            .unwrap();
    }
    let report = workspace
        .admit_sources(batch, SourceAdmissionPolicy::Strict, &mut budget)
        .unwrap();
    assert!(report.state_installed());
    assert_eq!(report.outcomes().len(), operation_count);
    assert_eq!(report.revision(), workspace.revision());
}

/// Test-only stand-in for the trusted project configuration that callers persist for reopen.
fn trusted_reopen_requests(view: &impl WorkspaceView) -> Vec<SourceOpenRequest> {
    let sources = view.sources(&mut AssetLoadBudget::default()).unwrap();
    sources
        .iter()
        .filter(|source| source.location() == SourceLocationKind::Root)
        .map(|source| {
            let mut request = SourceOpenRequest::new(
                source
                    .physical_origin()
                    .expect("committed root source must retain a physical origin"),
                source.locator().root_alias().clone(),
            )
            .with_kind_hint(source.kind());
            for companion in sources.iter().filter(|candidate| {
                candidate.parent() == Some(source.id())
                    && candidate.location() == SourceLocationKind::Companion
            }) {
                let member = companion
                    .locator()
                    .members()
                    .last()
                    .expect("companion locator must retain its member")
                    .member()
                    .clone();
                request = request.with_companion(SourceCompanionRequest::new(
                    companion
                        .physical_origin()
                        .expect("committed companion must retain a physical origin"),
                    member,
                ));
            }
            request
        })
        .collect()
}

fn streamed_payload(view: &impl WorkspaceView) -> Vec<u8> {
    let sidecar = view
        .sources(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .find(|source| source.kind() == SourceKind::StreamedResource)
        .expect("resource replacement must expose one streamed source");
    let range = view
        .read_source_range(
            sidecar.id(),
            0,
            REPLACEMENT_PAYLOAD.len() as u64,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut payload = Vec::new();
    range.reader().read_to_end(&mut payload).unwrap();
    payload
}

fn extraction_options() -> ExtractionExecutionOptions {
    ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(
            1,
            8 * 1024 * 1024,
            5,
            16 * 1024 * 1024,
            u64::MAX,
            8 * 1024 * 1024,
        )
        .unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap()
}

#[test]
fn public_structured_workflow_spans_mutation_recovery_extraction_and_search() {
    let temporary = common::secure_tempdir();
    let project_root = temporary.path();
    let source_path = project_root.join(SOURCE_ALIAS);
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    fs::write(&source_path, SOURCE).unwrap();

    let capabilities = serde_json::to_value(workspace_capabilities()).unwrap();
    assert_eq!(capabilities["automation"]["structured_input"], true);
    assert_eq!(capabilities["automation"]["display_text_input"], false);
    assert_eq!(capabilities["automation"]["generic_command_bus"], false);

    let mut workspace = AssetWorkspace::new().unwrap();
    admit_strict(
        &mut workspace,
        vec![
            SourceOpenRequest::new(&source_path, SourceAlias::new(SOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
        ],
    );
    let base = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&base);
    let WorkspaceLookup::Resolved(inspected) = inspector
        .object(&address("1"), &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("public inspector must resolve the fixture object");
    };
    assert_eq!(inspected.address(), &address("1"));
    assert_eq!(inspected.object().class().class_id(), 1);

    let planner = SchemaRecipePlanner::new(&base);
    let rename_target = planner
        .inspect(&address("1"), &mut AssetLoadBudget::default())
        .unwrap();
    let renamed = planner
        .lower_field_replace(
            &rename_target,
            field("m_Name"),
            MutationValue::string("After").unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let reference_target = planner
        .inspect(&address("2"), &mut AssetLoadBudget::default())
        .unwrap();
    let retargeted = planner
        .lower_reference(
            &reference_target,
            field("m_Texture"),
            ReferenceTarget::null(),
            ReferenceTarget::object(address("3")),
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
        .into_fragment()
        .expect("reference retarget must change the object");
    let resource_target = planner
        .inspect(&address("4"), &mut AssetLoadBudget::default())
        .unwrap();
    let resource = AudioClipResourceRecipe::lower(
        &planner,
        &resource_target,
        PlanPayload::new(REPLACEMENT_PAYLOAD.to_vec()),
        &mut AssetLoadBudget::default(),
    )
    .unwrap()
    .into_fragment()
    .expect("resource replacement must change the object");
    let hierarchy_intent = HierarchyIntentV1::for_view(
        &base,
        address("11"),
        HierarchyDestinationV1::parent(address("12"), HierarchyPlacementV1::Last),
    );
    let hierarchy =
        HierarchyRecipe::lower(&planner, &hierarchy_intent, &mut AssetLoadBudget::default())
            .unwrap()
            .into_fragment()
            .expect("hierarchy reparent must change the object");

    let mut builder = MutationPlanBuilder::new(base.workspace_id(), base.revision());
    builder.append(renamed).unwrap();
    builder.append(retargeted).unwrap();
    builder.append(resource).unwrap();
    builder.append(hierarchy).unwrap();
    let plan = builder.build().unwrap();
    let plan_json = plan.canonical_json().unwrap();
    let plan_value: serde_json::Value = serde_json::from_slice(&plan_json).unwrap();
    assert!(plan_value.get("command").is_none());
    assert_eq!(plan_value["operations"].as_array().unwrap().len(), 6);
    let plan = MutationPlan::from_json_slice(&plan_json, &mut AssetLoadBudget::default()).unwrap();

    let prepared = workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared_view = prepared.view();
    assert_eq!(
        value_at(&prepared_view, &address("1"), &field("m_Name")).as_str(),
        Some("After")
    );
    let pointer = value_at(&prepared_view, &address("2"), &field("m_Texture"));
    assert_eq!(
        pointer
            .as_object()
            .and_then(|fields| fields.get("fileID"))
            .and_then(UnityValue::as_i64),
        Some(3)
    );
    let stream = value_at(&prepared_view, &address("4"), &field("m_StreamData"));
    let stream_fields = stream.as_object().unwrap();
    assert_eq!(
        stream_fields.get("offset").and_then(UnityValue::as_u64),
        Some(0)
    );
    assert_eq!(
        stream_fields.get("size").and_then(UnityValue::as_u64),
        Some(REPLACEMENT_PAYLOAD.len() as u64)
    );
    assert_eq!(
        value_at(&base, &address("1"), &field("m_Name")).as_str(),
        Some("Before")
    );
    let child_parent = value_at(&prepared_view, &address("11"), &field("m_Father"));
    assert_eq!(
        child_parent
            .as_object()
            .and_then(|fields| fields.get("fileID"))
            .and_then(UnityValue::as_i64),
        Some(12)
    );
    assert!(
        value_at(&prepared_view, &address("10"), &field("m_Children"))
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    let new_parent_children = value_at(&prepared_view, &address("12"), &field("m_Children"));
    assert_eq!(
        new_parent_children
            .as_array()
            .and_then(|children| children.first())
            .and_then(UnityValue::as_object)
            .and_then(|fields| fields.get("fileID"))
            .and_then(UnityValue::as_i64),
        Some(11)
    );

    assert_eq!(streamed_payload(&prepared_view), REPLACEMENT_PAYLOAD);

    let prepared_revision = prepared_view.revision();
    let commit = workspace
        .commit(
            prepared,
            PublicationTarget::in_place(project_root).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(commit.committed_revision(), prepared_revision);
    assert_eq!(workspace.revision(), prepared_revision);
    let change_json = serde_json::to_value(commit.changes()).unwrap();
    assert!(change_json.get("command").is_none());
    assert_eq!(
        change_json["to_revision"],
        serde_json::json!(prepared_revision)
    );

    let committed = workspace.snapshot();
    let reopen_requests = trusted_reopen_requests(&committed);
    let workspace_id = workspace.workspace_id();
    let recovery = commit.recovery().clone();
    drop(committed);
    drop(prepared_view);
    drop(base);
    drop(workspace);

    let mut reopened_workspace =
        AssetWorkspace::with_workspace_id(workspace_id, WorkspaceOptions::default()).unwrap();
    admit_strict(&mut reopened_workspace, reopen_requests);
    assert_eq!(reopened_workspace.workspace_id(), workspace_id);
    assert_eq!(reopened_workspace.revision(), prepared_revision);
    assert_eq!(
        reopened_workspace.installation_digest(),
        commit.committed_installation()
    );
    let reopened = reopened_workspace.snapshot();
    assert_eq!(
        value_at(&reopened, &address("1"), &field("m_Name")).as_str(),
        Some("After")
    );
    assert_eq!(
        value_at(&reopened, &address("11"), &field("m_Father"),)
            .as_object()
            .and_then(|fields| fields.get("fileID"))
            .and_then(UnityValue::as_i64),
        Some(12)
    );
    assert_eq!(streamed_payload(&reopened), REPLACEMENT_PAYLOAD);

    let paths = IndexPaths::for_project(
        project_root.to_path_buf(),
        Some(project_root.join(".search-index")),
        Some(vec![PathBuf::from("Assets")]),
    )
    .unwrap();
    let index =
        SearchIndex::open_or_create(paths.clone(), &mut AssetLoadBudget::default()).unwrap();
    let receipt = index
        .reindex_workspace(
            commit.changes().clone(),
            &reopened,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    receipt.validate().unwrap();
    assert_eq!(receipt.disposition, ReindexDisposition::Applied);
    assert_eq!(receipt.target_revision, Some(prepared_revision));
    let generation = receipt
        .generation
        .clone()
        .expect("workspace reindex must publish an active generation");
    assert_eq!(generation.workspace, workspace_id);
    assert_eq!(generation.actual_revision, reopened.revision());
    assert_eq!(generation.desired_revision, reopened.revision());
    assert!(generation.semantics_current);
    assert!(generation.configuration_current);
    assert!(!generation.stale);
    assert_eq!(
        index.status().unwrap().generation.active.as_ref(),
        Some(&generation)
    );

    let search = index.search(SearchRequest::new("After", 20)).unwrap();
    assert_eq!(search.generation, generation);
    let renamed_hit = search
        .hits
        .iter()
        .find(|hit| hit.name == "After")
        .expect("search must expose the renamed source");
    assert_eq!(renamed_hit.location.path.as_str(), SOURCE_ALIAS);
    assert!(
        renamed_hit.location.file_id.is_none() && renamed_hit.location.class_id.is_none(),
        "search locations remain source-level; object identity comes from inspector and references"
    );
    let search_json = serde_json::to_value(&search).unwrap();
    assert!(search_json.get("command").is_none());

    let references = index
        .references(
            ReferenceRequest::incoming_object(address("3"), 20),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(references.generation, generation);
    assert!(references.hits.iter().any(|hit| {
        hit.location.file_id == Some(2)
            && hit
                .objects
                .iter()
                .any(|object| object.location.file_id == Some(3))
    }));
    let references_json = serde_json::to_value(&references).unwrap();
    assert!(references_json.get("command").is_none());

    let request =
        ExtractionRequest::addresses([address("4")], ExtractionRepresentationPolicy::RawOnly)
            .unwrap();
    let extraction_plan = ExtractionPlanner::new(&reopened)
        .plan(request, &mut AssetLoadBudget::default())
        .unwrap();
    let extraction_plan_json = extraction_plan.canonical_json().unwrap();
    let extraction_plan = ExtractionPlan::read_json(
        extraction_plan_json.as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    assert_eq!(extraction_plan.revision(), reopened.revision());
    let extraction = ExtractionExecutor::new()
        .execute(
            &reopened,
            &extraction_plan,
            &project_root.join("extracted"),
            ExtractionRunOptions::new(extraction_options()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(extraction.manifest().revision(), reopened.revision());
    assert_eq!(extraction.counts().written(), 1);
    let extraction_json = extraction.canonical_json().unwrap();
    assert_eq!(
        ExtractionReport::read_json(extraction_json.as_slice(), &mut AssetLoadBudget::default(),)
            .unwrap(),
        extraction
    );

    drop(index);
    let reopened_index =
        SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
    let reopened_status = reopened_index.status().unwrap();
    assert_eq!(
        reopened_status.generation.active.as_ref(),
        Some(&generation)
    );
    let reopened_search = reopened_index
        .search(SearchRequest::new("After", 20))
        .unwrap();
    assert_eq!(reopened_search.generation, generation);
    assert!(reopened_search.hits.iter().any(|hit| hit.name == "After"));
    drop(reopened_index);
    drop(reopened);
    drop(reopened_workspace);

    assert_eq!(
        AssetWorkspace::recover_at(&recovery, &mut AssetLoadBudget::default()).unwrap(),
        RecoveryOutcome::HistoricalCommitReceipt(Box::new(commit))
    );
}
