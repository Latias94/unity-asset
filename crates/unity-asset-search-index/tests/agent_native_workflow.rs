use std::fs;
use std::io::Read;
use std::path::PathBuf;

use unity_asset::extraction::{
    ExistingOutputPolicy, ExtractionExecutionLimits, ExtractionExecutionOptions,
    ExtractionExecutor, ExtractionFailurePolicy, ExtractionPlan, ExtractionPlanner,
    ExtractionReport, ExtractionRepresentationPolicy, ExtractionRequest, ExtractionRunOptions,
};
use unity_asset::schema::{AudioClipResourceRecipe, SchemaRecipePlanner};
use unity_asset::workspace::{
    AssetWorkspace, MutationPlan, MutationPlanBuilder, MutationValue, PlanPayload, PrepareOptions,
    PublicationTarget, RecoveryOutcome, ReferenceTarget, SourceOpenRequest, WorkspaceInspector,
    WorkspaceLookup, WorkspaceView, workspace_capabilities,
};
use unity_asset::{
    AssetLoadBudget, FieldPath, ObjectAddress, SourceAlias, SourceKind, SourceLocator, UnityValue,
};
use unity_asset_search_index::{IndexPaths, SearchIndex, SearchRequest};
use unity_asset_search_protocol::{ReferenceRequest, ReindexDisposition};

mod common;

const SOURCE_ALIAS: &str = "Assets/agent-native.asset";
const REPLACEMENT_PAYLOAD: &[u8] = b"OggS-agent-native-workflow";
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
  m_StreamData: {path: archive:/CAB-old/CAB-old.resS, offset: 7, size: 4}
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
    workspace
        .load_source(
            SourceOpenRequest::new(&source_path, SourceAlias::new(SOURCE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
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

    let mut builder = MutationPlanBuilder::new(base.workspace_id(), base.revision());
    builder.append(renamed).unwrap();
    builder.append(retargeted).unwrap();
    builder.append(resource).unwrap();
    let plan = builder.build().unwrap();
    let plan_json = plan.canonical_json().unwrap();
    let plan_value: serde_json::Value = serde_json::from_slice(&plan_json).unwrap();
    assert!(plan_value.get("command").is_none());
    assert_eq!(plan_value["operations"].as_array().unwrap().len(), 3);
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

    let sidecar = prepared_view
        .sources(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .find(|source| source.kind() == SourceKind::StreamedResource)
        .expect("prepared resource replacement must expose one sidecar");
    let range = prepared_view
        .read_source_range(
            sidecar.id(),
            0,
            REPLACEMENT_PAYLOAD.len() as u64,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut staged_payload = Vec::new();
    range.reader().read_to_end(&mut staged_payload).unwrap();
    assert_eq!(staged_payload, REPLACEMENT_PAYLOAD);

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
    let paths = IndexPaths::for_project(
        project_root.to_path_buf(),
        Some(project_root.join(".search-index")),
        Some(vec![PathBuf::from("Assets")]),
    )
    .unwrap();
    let index = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
    let receipt = index
        .reindex_workspace(
            commit.changes().clone(),
            &committed,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(receipt.disposition, ReindexDisposition::Applied);
    assert_eq!(receipt.target_revision, Some(prepared_revision));

    let search = index.search(SearchRequest::new("After", 20)).unwrap();
    assert_eq!(search.generation.actual_revision, prepared_revision);
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
    assert_eq!(references.generation.actual_revision, prepared_revision);
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
    let extraction_plan = ExtractionPlanner::new(&committed)
        .plan(request, &mut AssetLoadBudget::default())
        .unwrap();
    let extraction_plan_json = extraction_plan.canonical_json().unwrap();
    let extraction_plan = ExtractionPlan::read_json(
        extraction_plan_json.as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    assert_eq!(extraction_plan.revision(), prepared_revision);
    let extraction = ExtractionExecutor::new()
        .execute(
            &committed,
            &extraction_plan,
            &project_root.join("extracted"),
            ExtractionRunOptions::new(extraction_options()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(extraction.manifest().revision(), prepared_revision);
    assert_eq!(extraction.counts().written(), 1);
    let extraction_json = extraction.canonical_json().unwrap();
    assert_eq!(
        ExtractionReport::read_json(extraction_json.as_slice(), &mut AssetLoadBudget::default(),)
            .unwrap(),
        extraction
    );

    let recovery = commit.recovery().clone();
    let expected_commit = commit.clone();
    drop(index);
    drop(committed);
    drop(prepared_view);
    drop(base);
    drop(workspace);
    assert_eq!(
        AssetWorkspace::recover_at(&recovery, &mut AssetLoadBudget::default()).unwrap(),
        RecoveryOutcome::HistoricalCommitReceipt(Box::new(expected_commit))
    );
}
