use std::fs;
use std::io::{Cursor, Write};

use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationValue, PlanPayload,
    PrepareOptions, ResolvedStreamedResource, STREAMED_RESOURCE_QUERY_VERSION, SourceExpectation,
    StreamedResourceQueryResult, StreamedResourceRequest, StreamedResourceResolution,
    WorkspaceError, WorkspaceInspector, WorkspaceLookup, WorkspaceObjectFormatInspection,
    WorkspaceSourceFormatInspection, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, BudgetedJsonError, ContainmentKind,
    ContractError, FieldPath, ObjectAddress, SourceFingerprint, SourceId, SourceKind,
    SourceLocator, SourceMemberId,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

const SOURCE_ALIAS: &str = "inspection.prefab";
const YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: Before
"#;

const AUDIO_ALIAS: &str = "audio-clip.asset";
const AUDIO_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!83 &8300001
AudioClip:
  m_StreamData: {path: archive:/CAB-old/CAB-old.resS, offset: 7, size: 4}
"#;
const AUDIO_PAYLOAD: &[u8] = b"OggS-inspection-audio";

fn workspace_with_file(alias: &str, bytes: &[u8]) -> (tempfile::TempDir, AssetWorkspace, SourceId) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(alias);
    fs::write(&path, bytes).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    let source = workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    (directory, workspace, source)
}

fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, payload) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(payload).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn yaml_address(alias: &str, anchor: &str) -> ObjectAddress {
    ObjectAddress::yaml(SourceLocator::path(alias).unwrap(), anchor.parse().unwrap()).unwrap()
}

fn resolved_resource(result: &StreamedResourceQueryResult) -> &ResolvedStreamedResource {
    let StreamedResourceResolution::Resolved { resource } = result.resolution() else {
        panic!(
            "expected resolved streamed resource: {:?}",
            result.resolution()
        );
    };
    resource
}

fn field_replacement_plan(workspace: &AssetWorkspace) -> MutationPlan {
    let snapshot = workspace.snapshot();
    let address = yaml_address(SOURCE_ALIAS, "1");
    let path = FieldPath::root().push_field("m_Name").unwrap();
    let WorkspaceLookup::Resolved(handle) = snapshot
        .resolve_object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("inspection fixture object must resolve");
    };
    let object = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    let current = object.class().value_at_path(&path).unwrap();
    let guard = FieldGuard::new(
        yaml_field_schema_digest(
            object.class(),
            &path,
            current,
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
        semantic_value_digest(current, &mut AssetLoadBudget::default()).unwrap(),
    );
    MutationPlan::new(
        snapshot.workspace_id(),
        snapshot.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(SOURCE_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, YAML.as_bytes()),
        )],
        Vec::new(),
        vec![GenericMutation::FieldReplace {
            target: address,
            path,
            guard,
            replacement: MutationValue::string("After").unwrap(),
        }],
    )
    .unwrap()
}

fn resource_replacement_plan(workspace: &AssetWorkspace) -> MutationPlan {
    let snapshot = workspace.snapshot();
    let address = yaml_address(AUDIO_ALIAS, "8300001");
    let path = FieldPath::root().push_field("m_StreamData").unwrap();
    let WorkspaceLookup::Resolved(handle) = snapshot
        .resolve_object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("AudioClip fixture object must resolve");
    };
    let object = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    let current = object.class().value_at_path(&path).unwrap();
    let guard = FieldGuard::new(
        yaml_field_schema_digest(
            object.class(),
            &path,
            current,
            &mut AssetLoadBudget::default(),
        )
        .unwrap(),
        semantic_value_digest(current, &mut AssetLoadBudget::default()).unwrap(),
    );
    let payload = PlanPayload::new(AUDIO_PAYLOAD.to_vec());
    MutationPlan::new(
        snapshot.workspace_id(),
        snapshot.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(AUDIO_ALIAS).unwrap(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, AUDIO_YAML.as_bytes()),
        )],
        vec![payload.clone()],
        vec![GenericMutation::ResourceReplace {
            target: address,
            path,
            guard,
            payload: payload.digest(),
        }],
    )
    .unwrap()
}

#[test]
fn committed_inspection_projects_source_and_object_summaries() {
    let (_directory, workspace, source_id) = workspace_with_file(SOURCE_ALIAS, YAML.as_bytes());
    let snapshot = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&snapshot);

    let WorkspaceLookup::Resolved(source) = inspector
        .source(source_id, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("loaded YAML source must resolve");
    };
    assert_eq!(source.revision(), snapshot.revision());
    assert_eq!(source.source().id(), source_id);
    assert_eq!(source.source().kind(), SourceKind::Yaml);
    assert_eq!(
        source.source().locator(),
        &SourceLocator::path(SOURCE_ALIAS).unwrap()
    );
    assert_eq!(source.source().parent(), None);
    assert_eq!(source.parent_locator(), None);
    assert_eq!(source.encoded_length(), YAML.len() as u64);
    assert!(matches!(
        source.format(),
        WorkspaceSourceFormatInspection::Yaml { document_count: 1 }
    ));

    let address = yaml_address(SOURCE_ALIAS, "1");
    let WorkspaceLookup::Resolved(object) = inspector
        .object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("loaded YAML object must resolve");
    };
    assert_eq!(object.address(), &address);
    assert_eq!(object.object().handle().revision(), snapshot.revision());
    assert_eq!(object.object().handle().object().source(), source_id);
    assert_eq!(
        object.format(),
        WorkspaceObjectFormatInspection::Yaml { document_index: 0 }
    );
    assert_eq!(object.object().class().class_id(), 1);
    assert_eq!(object.object().class().class_name(), "GameObject");
    assert_eq!(
        object
            .object()
            .class()
            .value_at_path(&FieldPath::root().push_field("m_Name").unwrap())
            .unwrap()
            .as_str(),
        Some("Before")
    );
}

#[test]
fn prepared_inspection_is_revision_bound_and_reads_its_own_writes() {
    let (_directory, workspace, source_id) = workspace_with_file(SOURCE_ALIAS, YAML.as_bytes());
    let base = workspace.snapshot();
    let prepared = workspace
        .prepare(
            field_replacement_plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let view = prepared.view();
    let inspector = WorkspaceInspector::new(&view);

    assert_eq!(view.revision(), prepared.report().prepared_revision());
    assert_ne!(view.revision(), base.revision());
    let WorkspaceLookup::Resolved(source) = inspector
        .source(source_id, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("prepared YAML source must resolve");
    };
    assert_eq!(source.revision(), view.revision());
    assert_ne!(
        source.source().fingerprint(),
        SourceFingerprint::from_bytes(SourceKind::Yaml, YAML.as_bytes())
    );
    assert!(matches!(
        source.format(),
        WorkspaceSourceFormatInspection::Yaml { document_count: 1 }
    ));

    let address = yaml_address(SOURCE_ALIAS, "1");
    let WorkspaceLookup::Resolved(object) = inspector
        .object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("prepared YAML object must resolve");
    };
    assert_eq!(object.object().handle().revision(), view.revision());
    assert_eq!(
        object
            .object()
            .class()
            .value_at_path(&FieldPath::root().push_field("m_Name").unwrap())
            .unwrap()
            .as_str(),
        Some("After")
    );

    let base_inspector = WorkspaceInspector::new(&base);
    let WorkspaceLookup::Resolved(base_object) = base_inspector
        .object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("base YAML object must remain available");
    };
    assert_eq!(base_object.object().handle().revision(), base.revision());
    assert_eq!(
        base_object
            .object()
            .class()
            .value_at_path(&FieldPath::root().push_field("m_Name").unwrap())
            .unwrap()
            .as_str(),
        Some("Before")
    );
}

#[test]
fn prepared_stream_resolution_prefers_the_owners_direct_companion() {
    let (_directory, workspace, owner_id) = workspace_with_file(AUDIO_ALIAS, AUDIO_YAML.as_bytes());
    let prepared = workspace
        .prepare(
            resource_replacement_plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let view = prepared.view();
    let inspector = WorkspaceInspector::new(&view);
    let sources = inspector.sources(&mut AssetLoadBudget::default()).unwrap();
    let resources = sources
        .iter()
        .filter(|source| source.source().kind() == SourceKind::StreamedResource)
        .collect::<Vec<_>>();
    assert_eq!(resources.len(), 1);
    let companion = resources[0];
    assert_eq!(companion.source().parent(), Some(owner_id));
    assert_eq!(
        companion.parent_locator(),
        Some(&SourceLocator::path(AUDIO_ALIAS).unwrap())
    );
    assert!(matches!(
        companion.format(),
        WorkspaceSourceFormatInspection::StreamedResource
    ));
    let member = companion.source().locator().members().last().unwrap();
    assert_eq!(member.container(), ContainmentKind::Companion);

    let request = StreamedResourceRequest::new(
        SourceLocator::path(AUDIO_ALIAS).unwrap(),
        member.name(),
        0,
        AUDIO_PAYLOAD.len() as u64,
    )
    .unwrap();
    let result = inspector
        .resolve_streamed_resource(&request, &mut AssetLoadBudget::default())
        .unwrap();
    let resolved = resolved_resource(&result);
    assert_eq!(resolved.revision(), view.revision());
    assert_eq!(resolved.source().source_id(), companion.source().id());

    let range = resolved
        .open(&view, &mut AssetLoadBudget::default())
        .unwrap();
    let mut actual = Vec::new();
    range.copy_to(&mut actual).unwrap();
    assert_eq!(actual, AUDIO_PAYLOAD);
}

#[test]
fn committed_stream_resolution_finds_a_sibling_archive_sidecar() {
    let archive = zip_with_entries(&[
        ("scene.prefab", YAML.as_bytes()),
        ("CAB-sidecar.resS", b"0123456789"),
    ]);
    let (_directory, workspace, _archive_id) = workspace_with_file("sidecars.zip", &archive);
    let snapshot = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&snapshot);
    let owner = SourceLocator::archive_member("sidecars.zip", "scene.prefab").unwrap();
    let request =
        StreamedResourceRequest::new(owner, "archive:/CAB-sidecar/CAB-sidecar.resS", 2, 4).unwrap();

    let result = inspector
        .resolve_streamed_resource(&request, &mut AssetLoadBudget::default())
        .unwrap();
    let resolved = resolved_resource(&result);
    let member = resolved.source().locator().members().last().unwrap();
    assert_eq!(member.container(), ContainmentKind::Archive);
    assert_eq!(member.name(), "CAB-sidecar.resS");
    assert_eq!(
        resolved
            .open(&snapshot, &mut AssetLoadBudget::default())
            .unwrap()
            .contiguous(),
        Some(b"2345".as_slice())
    );
}

#[test]
fn streamed_resource_paths_disambiguate_exact_paths_and_report_case_ambiguity() {
    let archive = zip_with_entries(&[
        ("scene.prefab", YAML.as_bytes()),
        ("folder-a/shared.resS", b"AAAA"),
        ("folder-b/shared.resS", b"BBBB"),
        ("case/item.resS", b"lower"),
        ("CASE/ITEM.resS", b"upper"),
    ]);
    let (_directory, workspace, _archive_id) = workspace_with_file("paths.zip", &archive);
    let snapshot = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&snapshot);
    let owner = SourceLocator::archive_member("paths.zip", "scene.prefab").unwrap();

    let exact = StreamedResourceRequest::new(owner.clone(), "folder-b/shared.resS", 0, 4).unwrap();
    let exact_result = inspector
        .resolve_streamed_resource(&exact, &mut AssetLoadBudget::default())
        .unwrap();
    let exact_resource = resolved_resource(&exact_result);
    assert_eq!(
        exact_resource
            .source()
            .locator()
            .members()
            .last()
            .unwrap()
            .name(),
        "folder-b/shared.resS"
    );
    assert_eq!(
        exact_resource
            .open(&snapshot, &mut AssetLoadBudget::default())
            .unwrap()
            .contiguous(),
        Some(b"BBBB".as_slice())
    );

    let basename_only = StreamedResourceRequest::new(owner.clone(), "shared.resS", 0, 1).unwrap();
    let basename_result = inspector
        .resolve_streamed_resource(&basename_only, &mut AssetLoadBudget::default())
        .unwrap();
    let StreamedResourceResolution::Ambiguous { candidates } = basename_result.resolution() else {
        panic!(
            "basename collision must remain explicit: {:?}",
            basename_result.resolution()
        );
    };
    assert_eq!(candidates.len(), 2);

    let exact_case = StreamedResourceRequest::new(owner.clone(), "case/item.resS", 0, 1).unwrap();
    let exact_case_result = inspector
        .resolve_streamed_resource(&exact_case, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(
        resolved_resource(&exact_case_result)
            .source()
            .locator()
            .members()
            .last()
            .unwrap()
            .name(),
        "case/item.resS"
    );

    let case_only = StreamedResourceRequest::new(owner, "Case/Item.resS", 0, 1).unwrap();
    let case_result = inspector
        .resolve_streamed_resource(&case_only, &mut AssetLoadBudget::default())
        .unwrap();
    let StreamedResourceResolution::Ambiguous { candidates } = case_result.resolution() else {
        panic!(
            "case-only collision must remain explicit: {:?}",
            case_result.resolution()
        );
    };
    let names = candidates
        .iter()
        .map(|candidate| {
            candidate
                .locator()
                .members()
                .last()
                .unwrap()
                .name()
                .to_owned()
        })
        .collect::<Vec<_>>();
    let expected_names = inspector
        .sources(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .filter(|source| source.source().kind() == SourceKind::StreamedResource)
        .filter_map(|source| {
            source
                .source()
                .locator()
                .members()
                .last()
                .map(|member| member.name().to_owned())
        })
        .filter(|name| name.eq_ignore_ascii_case("Case/Item.resS"))
        .collect::<Vec<_>>();
    assert_eq!(
        names, expected_names,
        "ambiguous candidates must preserve workspace source order"
    );
}

#[test]
fn streamed_resource_resolution_keeps_missing_and_range_failures_distinct() {
    let archive = zip_with_entries(&[("scene.prefab", YAML.as_bytes()), ("scene.resS", b"four")]);
    let (_directory, workspace, _archive_id) = workspace_with_file("failures.zip", &archive);
    let snapshot = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&snapshot);
    let owner = SourceLocator::archive_member("failures.zip", "scene.prefab").unwrap();

    let missing = StreamedResourceRequest::new(owner.clone(), "missing.resS", 0, 1).unwrap();
    let missing_result = inspector
        .resolve_streamed_resource(&missing, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        missing_result.resolution(),
        StreamedResourceResolution::Missing
    ));

    let out_of_bounds = StreamedResourceRequest::new(owner, "scene.resS", 2, 3).unwrap();
    assert!(matches!(
        inspector.resolve_streamed_resource(&out_of_bounds, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::RangeOutOfBounds {
            offset: 2,
            end: 5,
            source_len: 4,
            ..
        })
    ));
}

#[test]
fn missing_stream_owner_does_not_spend_budget_on_global_resource_indexing() {
    let request = StreamedResourceRequest::new(
        SourceLocator::archive_member("resources.zip", "missing.prefab").unwrap(),
        "missing.resS",
        0,
        1,
    )
    .unwrap();

    let baseline_archive = zip_with_entries(&[("scene.prefab", YAML.as_bytes())]);
    let (_baseline_directory, baseline_workspace, _baseline_archive_id) =
        workspace_with_file("resources.zip", &baseline_archive);
    let baseline_snapshot = baseline_workspace.snapshot();
    let mut measured = AssetLoadBudget::default();
    let baseline = WorkspaceInspector::new(&baseline_snapshot)
        .resolve_streamed_resource(&request, &mut measured)
        .unwrap();
    assert!(matches!(
        baseline.resolution(),
        StreamedResourceResolution::OwnerMissing
    ));

    let archive = zip_with_entries(&[
        ("scene.prefab", YAML.as_bytes()),
        ("folder-a/shared.resS", b"AAAA"),
        ("folder-b/shared.resS", b"BBBB"),
    ]);
    let (_directory, populated_workspace, _archive_id) =
        workspace_with_file("resources.zip", &archive);
    let populated_snapshot = populated_workspace.snapshot();
    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: measured.usage().bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let result = WorkspaceInspector::new(&populated_snapshot)
        .resolve_streamed_resource(&request, &mut exact)
        .unwrap();

    assert!(matches!(
        result.resolution(),
        StreamedResourceResolution::OwnerMissing
    ));
    assert_eq!(exact.usage(), measured.usage());
}

#[test]
fn committed_and_prepared_views_use_the_same_streamed_resource_rules() {
    let (directory, workspace, _source_id) = workspace_with_file(SOURCE_ALIAS, YAML.as_bytes());
    let archive = zip_with_entries(&[
        ("scene.prefab", YAML.as_bytes()),
        ("folder-a/shared.resS", b"AAAA"),
        ("folder-b/shared.resS", b"BBBB"),
    ]);
    let archive_path = directory.path().join("consistent.zip");
    fs::write(&archive_path, archive).unwrap();
    let mut workspace = workspace;
    workspace
        .load_path(&archive_path, &mut AssetLoadBudget::default())
        .unwrap();

    let committed = workspace.snapshot();
    let prepared = workspace
        .prepare(
            field_replacement_plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let prepared_view = prepared.view();
    let request = StreamedResourceRequest::new(
        SourceLocator::archive_member("consistent.zip", "scene.prefab").unwrap(),
        "folder-b/shared.resS",
        0,
        4,
    )
    .unwrap();

    let committed_result = WorkspaceInspector::new(&committed)
        .resolve_streamed_resource(&request, &mut AssetLoadBudget::default())
        .unwrap();
    let prepared_result = WorkspaceInspector::new(&prepared_view)
        .resolve_streamed_resource(&request, &mut AssetLoadBudget::default())
        .unwrap();
    let committed_resource = resolved_resource(&committed_result);
    let prepared_resource = resolved_resource(&prepared_result);
    assert_eq!(
        committed_resource.source().source_id(),
        prepared_resource.source().source_id()
    );
    assert_eq!(
        committed_resource.source().locator(),
        prepared_resource.source().locator()
    );
    assert_eq!(
        committed_resource.source().fingerprint(),
        prepared_resource.source().fingerprint()
    );
}

#[test]
fn resolved_stream_ranges_revalidate_the_workspace_revision_before_opening() {
    let archive = zip_with_entries(&[
        ("scene.prefab", YAML.as_bytes()),
        ("scene.resS", b"revision-bound"),
    ]);
    let (directory, mut workspace, _archive_id) = workspace_with_file("revision.zip", &archive);
    let original = workspace.snapshot();
    let request = StreamedResourceRequest::new(
        SourceLocator::archive_member("revision.zip", "scene.prefab").unwrap(),
        "scene.resS",
        0,
        8,
    )
    .unwrap();
    let result = WorkspaceInspector::new(&original)
        .resolve_streamed_resource(&request, &mut AssetLoadBudget::default())
        .unwrap();
    let resolved = resolved_resource(&result);
    assert_eq!(
        resolved
            .open(&original, &mut AssetLoadBudget::default())
            .unwrap()
            .contiguous(),
        Some(b"revision".as_slice())
    );

    let additional_path = directory.path().join("additional.prefab");
    fs::write(&additional_path, YAML).unwrap();
    workspace
        .load_path(&additional_path, &mut AssetLoadBudget::default())
        .unwrap();
    let newer = workspace.snapshot();
    assert_ne!(newer.revision(), original.revision());
    assert!(matches!(
        resolved.open(&newer, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::Contract(ContractError::RevisionMismatch {
            expected,
            actual,
        })) if expected == original.revision() && actual == newer.revision()
    ));
}

#[test]
fn streamed_resource_request_json_accepts_exact_budget_and_rejects_one_short() {
    let request = StreamedResourceRequest::new(
        SourceLocator::archive_member("requests.zip", "scene.prefab").unwrap(),
        "archive:/CAB-request/CAB-request.resS",
        11,
        29,
    )
    .unwrap();
    let encoded = serde_json::to_vec(&request).unwrap();

    let mut measured = AssetLoadBudget::default();
    let decoded = StreamedResourceRequest::read_json(encoded.as_slice(), &mut measured).unwrap();
    assert_eq!(decoded, request);
    let usage = measured.usage();
    let parser_bytes = 4 * 1024 + 7 * u64::try_from(encoded.len()).unwrap();
    assert!(usage.bytes > parser_bytes);

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert_eq!(
        StreamedResourceRequest::read_json(encoded.as_slice(), &mut exact).unwrap(),
        request
    );
    assert_eq!(exact.usage(), usage);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        StreamedResourceRequest::read_json(encoded.as_slice(), &mut one_short),
        Err(BudgetedJsonError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));
    assert_eq!(one_short.usage().bytes, parser_bytes);
}

#[test]
fn streamed_resource_request_rejects_previous_and_future_wire_versions() {
    let request = StreamedResourceRequest::new(
        SourceLocator::path("scene.prefab").unwrap(),
        "scene.resS",
        0,
        1,
    )
    .unwrap();
    let current = serde_json::to_value(request).unwrap();

    for unsupported_version in [
        STREAMED_RESOURCE_QUERY_VERSION - 1,
        STREAMED_RESOURCE_QUERY_VERSION + 1,
    ] {
        let mut unsupported = current.clone();
        unsupported["version"] = serde_json::json!(unsupported_version);
        let encoded = serde_json::to_vec(&unsupported).unwrap();
        assert!(
            StreamedResourceRequest::read_json(
                encoded.as_slice(),
                &mut AssetLoadBudget::default(),
            )
            .is_err()
        );
    }
}

#[test]
fn streamed_resource_request_json_accepts_maximum_containment_owner() {
    let owner = (0..64)
        .try_fold(
            SourceLocator::path("root.assets").unwrap(),
            |owner, index| {
                owner.child(
                    ContainmentKind::Archive,
                    SourceMemberId::new(format!("member-{index}")).unwrap(),
                )
            },
        )
        .unwrap();
    let request = StreamedResourceRequest::new(owner, "scene.resS", 0, 1).unwrap();
    let encoded = serde_json::to_vec(&request).unwrap();

    assert_eq!(
        StreamedResourceRequest::read_json(encoded.as_slice(), &mut AssetLoadBudget::default(),)
            .unwrap(),
        request
    );
}

#[test]
fn streamed_resource_request_json_rejects_structure_beyond_its_contract_profile() {
    let nested = format!("{}0{}", "[".repeat(16), "]".repeat(16));
    let encoded = format!(r#"{{"unexpected":{nested}}}"#);
    let depth_error =
        StreamedResourceRequest::read_json(encoded.as_bytes(), &mut AssetLoadBudget::default())
            .unwrap_err();
    assert!(matches!(
        depth_error,
        BudgetedJsonError::StructureLimitExceeded {
            contract: "unity_asset.streamed_resource_request",
            resource: "depth",
            limit: 16,
            requested: 17,
        }
    ));

    let fields = (0..512)
        .map(|index| format!(r#""field_{index}":0"#))
        .collect::<Vec<_>>()
        .join(",");
    let encoded = format!("{{{fields}}}");
    let width_error =
        StreamedResourceRequest::read_json(encoded.as_bytes(), &mut AssetLoadBudget::default())
            .unwrap_err();
    assert!(matches!(
        width_error,
        BudgetedJsonError::StructureLimitExceeded {
            contract: "unity_asset.streamed_resource_request",
            resource: "entries",
            limit: 512,
            requested: 513,
        }
    ));
}

#[test]
fn streamed_resource_request_json_rejects_input_above_raw_contract_cap() {
    let encoded = vec![b' '; 64 * 1024 + 1];
    let error =
        StreamedResourceRequest::read_json(encoded.as_slice(), &mut AssetLoadBudget::default())
            .unwrap_err();

    assert!(matches!(
        error,
        BudgetedJsonError::EncodedLimitExceeded {
            contract: "unity_asset.streamed_resource_request",
            limit: 65_536,
            requested: 65_537,
        }
    ));
}

#[test]
fn streamed_resource_request_json_rejects_trailing_document() {
    let request = StreamedResourceRequest::new(
        SourceLocator::path("scene.prefab").unwrap(),
        "scene.resS",
        0,
        1,
    )
    .unwrap();
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.extend_from_slice(b"\n{}");

    assert!(matches!(
        StreamedResourceRequest::read_json(encoded.as_slice(), &mut AssetLoadBudget::default(),),
        Err(BudgetedJsonError::Json(_))
    ));
}

#[test]
fn streamed_resource_request_rejects_empty_identity_and_zero_ranges() {
    let owner = SourceLocator::path("scene.assets").unwrap();

    assert_eq!(
        StreamedResourceRequest::new(owner.clone(), "   ", 0, 1),
        Err(unity_asset::workspace::StreamedResourceRequestError::EmptyPath)
    );
    assert_eq!(
        StreamedResourceRequest::new(owner.clone(), "archive:/", 0, 1),
        Err(unity_asset::workspace::StreamedResourceRequestError::InvalidBasename)
    );
    assert_eq!(
        StreamedResourceRequest::new(owner, "scene.resS", 0, 0),
        Err(unity_asset::workspace::StreamedResourceRequestError::ZeroSize)
    );
}
