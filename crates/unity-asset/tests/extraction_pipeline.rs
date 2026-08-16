use std::fs;
use std::mem::size_of;

#[cfg(not(feature = "decode"))]
use unity_asset::extraction::ExtractionFilter;
use unity_asset::extraction::{
    BundleContainerQuery, BundleContainerResolution, EXTRACTION_PLAN_VERSION, ExistingOutputPolicy,
    ExtractionArtifactKind, ExtractionArtifactStatus, ExtractionDiagnosticCode,
    ExtractionExecutionError, ExtractionExecutionLimits, ExtractionExecutionOptions,
    ExtractionExecutor, ExtractionFailurePolicy, ExtractionManifest, ExtractionPath,
    ExtractionPlan, ExtractionPlanError, ExtractionPlanMismatchKind, ExtractionPlanner,
    ExtractionReport, ExtractionRepresentationPolicy, ExtractionRequest, ExtractionRunOptions,
};
use unity_asset::reference::{RawReferenceTarget, ReferenceGraphBuildOptions};
use unity_asset::schema::SchemaRecipePlanner;
use unity_asset::workspace::{
    AssetWorkspace, MutationPlanBuilder, MutationValue, PrepareOptions, WorkspaceError,
    WorkspaceLookup, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, BudgetedJsonError, DigestV1, FieldPath,
    ObjectAddress, ObjectKind, SourceLocator,
};
use unity_asset_binary::asset::{SerializedFileParser, class_ids};
use unity_asset_binary::bundle::BundleParser;

#[path = "support/source_replacement.rs"]
mod source_replacement;

const FIRST_SOURCE: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1001
GameObject:
  m_Name: Alpha
  m_IsActive: 1
--- !u!114 &1002
MonoBehaviour:
  m_Name: Beta
  m_Enabled: 1
"#;

const SECOND_SOURCE: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1001
GameObject:
  m_Name: Changed
  m_IsActive: 0
"#;

fn sample(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/samples")
        .join(name)
}

fn serialized_file_with_duplicate_container_target() -> (Vec<u8>, [String; 2]) {
    let bundle = BundleParser::from_bytes(fs::read(sample("char_118_yuki.ab")).unwrap()).unwrap();
    let node = bundle
        .nodes
        .iter()
        .find(|node| node.is_serialized_file())
        .expect("fixture bundle must contain a SerializedFile");
    let mut image = bundle.extract_node_data(node).unwrap();
    let serialized = SerializedFileParser::from_bytes(image.clone()).unwrap();
    assert_eq!(serialized.header.endian, 0, "fixture must be little-endian");
    let bundle_object = serialized
        .objects()
        .iter()
        .find(|object| object.class_id() == class_ids::ASSET_BUNDLE)
        .expect("fixture SerializedFile must contain an AssetBundle object");
    let entries = serialized
        .assetbundle_container_raw(bundle_object, &mut AssetLoadBudget::default())
        .unwrap();
    let first = entries.first().cloned().expect("fixture container entry");
    let second = entries
        .get(1)
        .cloned()
        .expect("second fixture container entry");
    assert_ne!(first.2, second.2);

    let object_start = usize::try_from(bundle_object.byte_start()).unwrap();
    let object_end = usize::try_from(bundle_object.byte_end().unwrap()).unwrap();
    let object = &image[object_start..object_end];
    let first_offset = container_path_id_offset(object, &first.0, first.1, first.2);
    let second_offset = container_path_id_offset(object, &second.0, second.1, second.2);
    assert_ne!(first_offset, second_offset);

    let second_start = object_start + second_offset;
    image[second_start..second_start + size_of::<i64>()].copy_from_slice(&first.2.to_le_bytes());

    let reparsed = SerializedFileParser::from_bytes(image.clone()).unwrap();
    let reparsed_bundle = reparsed
        .objects()
        .iter()
        .find(|object| object.class_id() == class_ids::ASSET_BUNDLE)
        .unwrap();
    let reparsed_entries = reparsed
        .assetbundle_container_raw(reparsed_bundle, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(reparsed_entries[0].2, reparsed_entries[1].2);

    (image, [first.0, second.0])
}

fn container_path_id_offset(object: &[u8], asset_path: &str, file_id: i32, path_id: i64) -> usize {
    let path = asset_path.as_bytes();
    let encoded_length = i32::try_from(path.len()).unwrap().to_le_bytes();
    let encoded_file_id = file_id.to_le_bytes();
    let encoded_path_id = path_id.to_le_bytes();
    let mut matches = Vec::new();

    for (path_start, candidate) in object.windows(path.len()).enumerate() {
        if candidate != path || path_start < size_of::<i32>() {
            continue;
        }
        if object[path_start - size_of::<i32>()..path_start] != encoded_length {
            continue;
        }

        let aligned_end = (path_start + path.len() + 3) & !3;
        for path_id_offset in [
            aligned_end + size_of::<i32>(),
            aligned_end + 3 * size_of::<i32>(),
        ] {
            let file_id_offset = path_id_offset - size_of::<i32>();
            let Some(encoded_pointer) =
                object.get(file_id_offset..path_id_offset + size_of::<i64>())
            else {
                continue;
            };
            if encoded_pointer[..size_of::<i32>()] == encoded_file_id
                && encoded_pointer[size_of::<i32>()..] == encoded_path_id
            {
                matches.push(path_id_offset);
            }
        }
    }

    assert_eq!(
        matches.len(),
        1,
        "fixture entry {asset_path:?} must have one unambiguous pointer"
    );
    matches[0]
}

fn options(workers: usize, existing: ExistingOutputPolicy) -> ExtractionExecutionOptions {
    options_with_failure(workers, existing, ExtractionFailurePolicy::CollectAll)
}

fn options_with_failure(
    workers: usize,
    existing: ExistingOutputPolicy,
    failure: ExtractionFailurePolicy,
) -> ExtractionExecutionOptions {
    ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(
            workers,
            8 * 1024 * 1024,
            workers.saturating_mul(2).saturating_add(1).max(5),
            32 * 1024 * 1024,
            u64::MAX,
            8 * 1024 * 1024,
        )
        .unwrap(),
        existing,
        failure,
    )
    .unwrap()
}

fn assert_no_staging_files(root: &std::path::Path) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            assert_no_staging_files(&path);
        } else {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !(name.starts_with(".unity-asset-") && name.ends_with(".tmp")),
                "staging output leaked at {}",
                path.display()
            );
        }
    }
}

#[test]
fn yaml_document_request_filters_mixed_workspaces_and_publishes_a_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let yaml_path = directory.path().join("objects.prefab");
    let output = directory.path().join("output");
    fs::write(&yaml_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&yaml_path, &mut AssetLoadBudget::default())
        .unwrap();
    workspace
        .load_path(sample("banner_1"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let request =
        ExtractionRequest::yaml_documents().with_prefix(ExtractionPath::new("documents").unwrap());
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(request, &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(plan.artifacts().len(), 2);
    assert!(plan.artifacts().iter().all(|artifact| {
        artifact.address().kind() == ObjectKind::Yaml
            && artifact.preferred_kind() == ExtractionArtifactKind::Yaml
            && artifact.preferred_path().as_str().contains("/file-id-")
            && !artifact.preferred_path().as_str().contains("/alpha--")
            && !artifact.preferred_path().as_str().contains("/beta--")
    }));

    let manifest_path = ExtractionPath::new("extraction-manifest.json").unwrap();
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error))
                .with_manifest_path(&manifest_path),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(report.counts().written(), 2);
    assert!(output.join(manifest_path.as_str()).is_file());
    assert!(
        report
            .manifest()
            .artifacts()
            .iter()
            .all(|artifact| artifact.kind() == ExtractionArtifactKind::Yaml)
    );
    assert_no_staging_files(&output);
}

#[test]
fn persisted_yaml_plan_rejects_an_object_kind_filter_downgrade() {
    let directory = tempfile::tempdir().unwrap();
    let yaml_path = directory.path().join("objects.prefab");
    fs::write(&yaml_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&yaml_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::yaml_documents(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut wire = serde_json::to_value(&plan).unwrap();
    wire["request"]["filter"]["object_kinds"] = serde_json::json!(["binary"]);
    let request: ExtractionRequest = serde_json::from_value(wire["request"].clone()).unwrap();
    wire["request_digest"] = serde_json::to_value(request.digest().unwrap()).unwrap();

    let error = serde_json::from_value::<ExtractionPlan>(wire).unwrap_err();
    assert!(error.to_string().contains("request filter excludes"));
}

#[cfg(not(feature = "decode"))]
#[test]
fn require_decoded_reports_feature_unavailable_without_decode_support() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("banner_1"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
        .with_filter(ExtractionFilter::new([28], None, None, None).unwrap());

    let error = ExtractionPlanner::new(&snapshot)
        .plan(request, &mut AssetLoadBudget::default())
        .unwrap_err();

    assert!(matches!(
        error,
        ExtractionPlanError::RequiredDecodedUnavailable { .. }
    ));
}

#[test]
fn extraction_plan_finalization_obeys_exact_and_one_short_budgets() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let planner = ExtractionPlanner::new(&snapshot);

    let mut measured = AssetLoadBudget::default();
    let expected = planner
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut measured,
        )
        .unwrap();
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
    let actual = planner
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut exact,
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(exact.usage(), usage);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..exact_limits
    })
    .unwrap();
    let error = planner
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut one_short,
        )
        .unwrap_err();
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
}

#[test]
fn stop_in_plan_order_discards_every_later_staged_output() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("output");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let first = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let first_path = output.join(first.manifest().artifacts()[0].path().as_str());
    let later_path = output.join(first.manifest().artifacts()[1].path().as_str());
    fs::remove_file(&later_path).unwrap();

    let stopped = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options_with_failure(
                2,
                ExistingOutputPolicy::Error,
                ExtractionFailurePolicy::StopInPlanOrder,
            )),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert!(first_path.exists());
    assert!(!later_path.exists());
    assert_eq!(stopped.counts().failed(), 2);
    assert_eq!(
        stopped.manifest().artifacts()[0].diagnostics()[0].code(),
        ExtractionDiagnosticCode::OutputExists
    );
    assert_eq!(
        stopped.manifest().artifacts()[1].diagnostics()[0].code(),
        ExtractionDiagnosticCode::StoppedAfterFailure
    );
}

#[test]
fn planning_is_write_free_and_worker_count_does_not_change_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let one_output = directory.path().join("one");
    let many_output = directory.path().join("many");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(plan.artifacts().len(), 2);
    assert!(!one_output.exists());
    assert!(!many_output.exists());

    let one = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &one_output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let many = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &many_output,
            ExtractionRunOptions::new(options(4, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let open_file_limited = ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(
            4,
            8 * 1024 * 1024,
            5,
            32 * 1024 * 1024,
            u64::MAX,
            8 * 1024 * 1024,
        )
        .unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap();
    let open_file_limited_report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &directory.path().join("open-file-limited"),
            ExtractionRunOptions::new(open_file_limited),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(
        one.canonical_manifest_json().unwrap(),
        many.canonical_manifest_json().unwrap()
    );
    assert_eq!(
        one.canonical_manifest_json().unwrap(),
        open_file_limited_report.canonical_manifest_json().unwrap(),
        "an open-file cap must only change scheduling, not canonical results"
    );
    assert_eq!(one.counts().written(), 2);
    for artifact in one.manifest().artifacts() {
        assert_eq!(artifact.status(), ExtractionArtifactStatus::Written);
        assert_eq!(
            fs::read(one_output.join(artifact.path().as_str())).unwrap(),
            fs::read(many_output.join(artifact.path().as_str())).unwrap(),
        );
    }

    let encoded = one.canonical_manifest_json().unwrap();
    let decoded =
        ExtractionManifest::read_json(encoded.as_slice(), &mut AssetLoadBudget::default()).unwrap();
    assert_eq!(&decoded, one.manifest());
    let mut legacy_manifest: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    legacy_manifest["version"] = serde_json::Value::from(2);
    let error = ExtractionManifest::read_json(
        serde_json::to_vec(&legacy_manifest).unwrap().as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BudgetedJsonError::Json(source)
            if source.to_string().contains("manifest version 2 is unsupported")
    ));

    let report_encoded = one.canonical_json().unwrap();
    let report_decoded =
        ExtractionReport::read_json(report_encoded.as_slice(), &mut AssetLoadBudget::default())
            .unwrap();
    assert_eq!(report_decoded, one);
    assert_eq!(one.digest().unwrap(), DigestV1::hash_bytes(&report_encoded));
    let mut legacy_report: serde_json::Value = serde_json::from_slice(&report_encoded).unwrap();
    legacy_report["version"] = serde_json::Value::from(2);
    let error = ExtractionReport::read_json(
        serde_json::to_vec(&legacy_report).unwrap().as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BudgetedJsonError::Json(source)
            if source.to_string().contains("report version 2 is unsupported")
    ));

    legacy_report["manifest"]["version"] = serde_json::Value::from(2);
    let error = ExtractionReport::read_json(
        serde_json::to_vec(&legacy_report).unwrap().as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BudgetedJsonError::Json(source)
            if source.to_string().contains("report version 2 is unsupported")
    ));

    let canonical_report: serde_json::Value = serde_json::from_slice(&report_encoded).unwrap();
    for field in ["written", "resumed", "skipped_existing", "failed"] {
        let mut tampered = canonical_report.clone();
        let count = tampered["counts"][field].as_u64().unwrap();
        tampered["counts"][field] = serde_json::Value::from(count.checked_add(1).unwrap());
        let tampered = serde_json::to_vec(&tampered).unwrap();

        assert!(
            ExtractionReport::read_json(tampered.as_slice(), &mut AssetLoadBudget::default())
                .is_err(),
            "tampered {field} count must be rejected"
        );
    }
}

#[test]
fn prepared_view_extraction_reads_the_uncommitted_candidate_revision() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("prepared-output");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let base = workspace.snapshot();
    let address = ObjectAddress::yaml(
        SourceLocator::path("objects.prefab").unwrap(),
        "1001".parse().unwrap(),
    )
    .unwrap();
    let name_path = FieldPath::root().push_field("m_Name").unwrap();

    let recipes = SchemaRecipePlanner::new(&base);
    let observed = recipes
        .inspect(&address, &mut AssetLoadBudget::default())
        .unwrap();
    let fragment = recipes
        .lower_field_replace(
            &observed,
            name_path.clone(),
            MutationValue::string("Prepared").unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut builder = MutationPlanBuilder::new(base.workspace_id(), base.revision());
    builder.append(fragment).unwrap();
    let prepared = workspace
        .prepare(
            builder.build().unwrap(),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let view = prepared.view();

    let plan = ExtractionPlanner::new(&view)
        .plan(
            ExtractionRequest::addresses(
                [address.clone()],
                ExtractionRepresentationPolicy::RawOnly,
            )
            .unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(plan.revision(), view.revision());
    assert_eq!(plan.artifacts().len(), 1);
    assert_eq!(plan.artifacts()[0].object_name(), Some("Prepared"));

    let report = ExtractionExecutor::new()
        .execute(
            &view,
            &plan,
            &output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let artifact = report
        .manifest()
        .artifact_by_address(&address)
        .expect("prepared object must have one extraction receipt");
    assert_eq!(artifact.status(), ExtractionArtifactStatus::Written);
    assert_eq!(report.manifest().revision(), view.revision());
    let yaml = fs::read_to_string(output.join(artifact.path().as_str())).unwrap();
    assert!(yaml.contains("m_Name: Prepared"));
    assert!(!yaml.contains("m_Name: Alpha"));

    assert_eq!(fs::read_to_string(&source_path).unwrap(), FIRST_SOURCE);
    let WorkspaceLookup::Resolved(base_handle) = base
        .resolve_object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("base object must remain resolvable");
    };
    let base_object = base
        .read_object(&base_handle, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(
        base_object
            .class()
            .value_at_path(&name_path)
            .unwrap()
            .as_str(),
        Some("Alpha")
    );
}

#[test]
fn corrupted_resume_output_requires_explicit_replacement_authority() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("output");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let first = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let corrupted = &first.manifest().artifacts()[0];
    let corrupted_path = output.join(corrupted.path().as_str());
    let correct_bytes = fs::read(&corrupted_path).unwrap();
    let mut bytes = correct_bytes.clone();
    bytes[0] ^= 0xff;
    fs::write(&corrupted_path, &bytes).unwrap();

    let rejected = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Error))
                .with_resume(first.manifest()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(rejected.counts().failed(), 1);
    assert_eq!(rejected.counts().resumed(), 1);
    assert_eq!(fs::read(&corrupted_path).unwrap(), bytes);

    let resumed = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Replace))
                .with_resume(first.manifest()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(resumed.counts().written(), 1);
    assert_eq!(resumed.counts().resumed(), 1);
    let rebuilt = &resumed.manifest().artifacts()[0];
    assert_eq!(rebuilt.status(), ExtractionArtifactStatus::Written);
    assert_eq!(rebuilt.digest(), corrupted.digest());
    let actual = fs::read(&corrupted_path).unwrap();
    assert_eq!(actual, correct_bytes);
    assert_eq!(rebuilt.digest(), Some(DigestV1::hash_bytes(&actual)));
    assert_eq!(
        u64::try_from(actual.len()).unwrap(),
        corrupted.length().unwrap()
    );
}

#[test]
fn existing_output_policies_produce_stable_receipts_without_changing_files() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("output");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let first = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let original = first
        .manifest()
        .artifacts()
        .iter()
        .map(|artifact| fs::read(output.join(artifact.path().as_str())).unwrap())
        .collect::<Vec<_>>();

    let skipped = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Skip)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(skipped.counts().skipped_existing(), 2);

    let rejected = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(rejected.counts().failed(), 2);
    for (index, artifact) in rejected.manifest().artifacts().iter().enumerate() {
        assert_eq!(artifact.status(), ExtractionArtifactStatus::Failed);
        assert_eq!(
            fs::read(output.join(artifact.path().as_str())).unwrap(),
            original[index],
        );
    }

    for artifact in first.manifest().artifacts() {
        fs::write(output.join(artifact.path().as_str()), b"stale output").unwrap();
    }
    let replaced = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Replace)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(replaced.counts().written(), 2);
    for (index, artifact) in replaced.manifest().artifacts().iter().enumerate() {
        assert_eq!(artifact.status(), ExtractionArtifactStatus::Written);
        assert_eq!(
            fs::read(output.join(artifact.path().as_str())).unwrap(),
            original[index]
        );
    }
    assert_no_staging_files(&output);
}

#[test]
fn working_set_and_report_bounds_reject_before_creating_output() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let working_set_limited = ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(2, 1, 5, 32 * 1024 * 1024, u64::MAX, 8 * 1024 * 1024)
            .unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap();
    let working_set_output = directory.path().join("working-set-limited");
    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &working_set_output,
            ExtractionRunOptions::new(working_set_limited),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractionExecutionError::WorkingSetExceedsLimit { .. }
    ));
    assert!(!working_set_output.exists());

    let report_limited = ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(2, 8 * 1024 * 1024, 5, 32 * 1024 * 1024, u64::MAX, 1)
            .unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap();
    let report_output = directory.path().join("report-limited");
    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &report_output,
            ExtractionRunOptions::new(report_limited),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractionExecutionError::ReportLimitExceeded { .. }
    ));
    assert!(!report_output.exists());
}

#[test]
fn persisted_plan_cannot_understate_its_authoritative_working_set() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("underdeclared");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut wire: serde_json::Value =
        serde_json::from_slice(&plan.canonical_json().unwrap()).unwrap();
    wire["artifacts"][0]["working_set_bytes"] = serde_json::Value::from(1);
    let tampered = serde_json::to_vec(&wire).unwrap();
    let plan =
        ExtractionPlan::read_json(tampered.as_slice(), &mut AssetLoadBudget::default()).unwrap();

    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(
        matches!(
            &error,
            ExtractionExecutionError::PlanVerification(source)
                if matches!(
                    source.as_ref(),
                    ExtractionPlanError::PlanDerivationMismatch {
                        kind: ExtractionPlanMismatchKind::Representations,
                    }
                )
        ),
        "unexpected error: {error:?}"
    );
    assert!(!output.exists());
}

#[test]
fn persisted_plan_request_must_rederive_the_exact_artifact_set() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("request-mismatch");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(plan.artifacts().len(), 2);

    let mut wire: serde_json::Value =
        serde_json::from_slice(&plan.canonical_json().unwrap()).unwrap();
    let mut prefix_wire = wire.clone();
    prefix_wire["request"]["prefix"] = serde_json::json!("relocated");
    let prefix_request: ExtractionRequest =
        serde_json::from_value(prefix_wire["request"].clone()).unwrap();
    prefix_wire["request_digest"] = serde_json::to_value(prefix_request.digest().unwrap()).unwrap();
    let error = ExtractionPlan::read_json(
        serde_json::to_vec(&prefix_wire).unwrap().as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("outside the extraction request prefix")
    );

    let mut nested_prefix_wire = wire.clone();
    nested_prefix_wire["request"]["prefix"] = serde_json::json!("sources");
    let nested_prefix_request: ExtractionRequest =
        serde_json::from_value(nested_prefix_wire["request"].clone()).unwrap();
    nested_prefix_wire["request_digest"] =
        serde_json::to_value(nested_prefix_request.digest().unwrap()).unwrap();
    let nested_prefix_plan = ExtractionPlan::read_json(
        serde_json::to_vec(&nested_prefix_wire).unwrap().as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &nested_prefix_plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractionExecutionError::PlanVerification(source)
            if matches!(
                source.as_ref(),
                ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::ArtifactPaths,
                }
            )
    ));
    assert!(!output.exists());

    let mut legacy = wire.clone();
    legacy["version"] = serde_json::Value::from(EXTRACTION_PLAN_VERSION - 1);
    let legacy = serde_json::to_vec(&legacy).unwrap();
    let error =
        ExtractionPlan::read_json(legacy.as_slice(), &mut AssetLoadBudget::default()).unwrap_err();
    assert!(error.to_string().contains(&format!(
        "version {} is unsupported",
        EXTRACTION_PLAN_VERSION - 1
    )));

    wire["request"]["filter"]["limit"] = serde_json::Value::from(1);
    let request: ExtractionRequest = serde_json::from_value(wire["request"].clone()).unwrap();
    wire["request_digest"] = serde_json::to_value(request.digest().unwrap()).unwrap();
    let tampered = serde_json::to_vec(&wire).unwrap();
    let plan =
        ExtractionPlan::read_json(tampered.as_slice(), &mut AssetLoadBudget::default()).unwrap();

    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(2, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ExtractionExecutionError::PlanVerification(source)
            if matches!(
                source.as_ref(),
                ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::Artifacts,
                }
            )
    ));
    assert!(!output.exists());
}

#[test]
fn open_file_limit_reserves_lock_and_verified_publication_handles() {
    let error = ExtractionExecutionLimits::new(
        1,
        8 * 1024 * 1024,
        4,
        32 * 1024 * 1024,
        u64::MAX,
        8 * 1024 * 1024,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        ExtractionExecutionError::OpenFileLimitTooSmall {
            minimum: 5,
            limit: 4
        }
    ));
}

#[test]
fn durable_manifest_path_cannot_collide_with_a_planned_artifact() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("output");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let manifest_path = plan.artifacts()[0].preferred_path();

    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error))
                .with_manifest_path(manifest_path),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ExtractionExecutionError::OutputLayout { .. }
    ));
    assert!(!output.exists());
}

#[test]
fn manifest_output_reservation_fails_before_creating_the_output_root() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("output");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let limited = ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(1, 8 * 1024 * 1024, 5, 1, u64::MAX, 8 * 1024 * 1024)
            .unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap();
    let manifest_path = ExtractionPath::new("manifest.json").unwrap();

    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(limited).with_manifest_path(&manifest_path),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ExtractionExecutionError::ManifestOutputLimitExceeded { limit: 1, .. }
    ));
    assert!(!output.exists());
}

#[test]
fn output_limit_rejects_artifacts_before_publication() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("output");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let limited = ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::new(2, 8 * 1024 * 1024, 5, 1, u64::MAX, 8 * 1024 * 1024)
            .unwrap(),
        ExistingOutputPolicy::Error,
        ExtractionFailurePolicy::CollectAll,
    )
    .unwrap();

    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(limited),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(report.counts().failed(), 2);
    for artifact in report.manifest().artifacts() {
        assert_eq!(artifact.status(), ExtractionArtifactStatus::Failed);
        assert_eq!(
            artifact.diagnostics()[0].code(),
            ExtractionDiagnosticCode::OutputLimitExceeded
        );
        assert!(!output.join(artifact.path().as_str()).exists());
    }
}

#[test]
fn revision_mismatch_fails_before_creating_the_output_root() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("objects.prefab");
    let output = directory.path().join("output");
    fs::write(&source_path, FIRST_SOURCE).unwrap();

    let mut workspace = AssetWorkspace::new().unwrap();
    let source = workspace
        .load_path(&source_path, &mut AssetLoadBudget::default())
        .unwrap();
    let old_snapshot = workspace.snapshot();
    let plan = ExtractionPlanner::new(&old_snapshot)
        .plan(
            ExtractionRequest::all(ExtractionRepresentationPolicy::RawOnly),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    fs::write(&source_path, SECOND_SOURCE).unwrap();
    source_replacement::replace_source_path(&mut workspace, source, &source_path, "objects.prefab");
    let new_snapshot = workspace.snapshot();
    let error = ExtractionExecutor::new()
        .execute(
            &new_snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ExtractionExecutionError::WorkspaceContextMismatch
    ));
    assert!(!output.exists());
}

#[test]
fn bundle_container_query_preserves_same_target_occurrences_and_exact_budget() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("duplicate-target.assets");
    let (image, asset_paths) = serialized_file_with_duplicate_container_target();
    fs::write(&source_path, image).unwrap();

    let query = |budget: &mut AssetLoadBudget| {
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_path(&source_path, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        ExtractionPlanner::new(&snapshot)
            .bundle_container_occurrences(BundleContainerQuery::new("*").unwrap(), budget)
    };

    let mut measured = AssetLoadBudget::default();
    let result = query(&mut measured).unwrap();
    let (first_index, first) = result
        .occurrences()
        .iter()
        .enumerate()
        .find(|(_, occurrence)| occurrence.asset_path() == asset_paths[0])
        .expect("first patched occurrence");
    let (second_index, second) = result
        .occurrences()
        .iter()
        .enumerate()
        .find(|(_, occurrence)| occurrence.asset_path() == asset_paths[1])
        .expect("second patched occurrence");
    assert_ne!(first.ordinal(), second.ordinal());
    assert_ne!(first.field_path(), second.field_path());
    assert!(first_index < second_index);
    assert!(first.ordinal() < second.ordinal());
    assert_eq!(
        first.resolution().resolved(),
        second.resolution().resolved()
    );
    assert!(first.resolution().resolved().is_some());

    let usage = measured.usage();
    assert!(usage.bytes > 1);
    let exact_limits = AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        max_depth: usage.max_observed_depth,
        max_members: usage.members,
        ..AssetLoadLimits::default()
    };
    let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
    let exact_result = query(&mut exact).unwrap();
    assert_eq!(exact_result.query(), result.query());
    assert_eq!(exact_result.is_complete(), result.is_complete());
    assert_eq!(exact_result.occurrences(), result.occurrences());
    assert_eq!(exact.usage(), usage);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..exact_limits
    })
    .unwrap();
    let error = query(&mut one_short).unwrap_err();
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
}

#[test]
fn bundle_container_and_explicit_handle_publish_identical_artifact_bytes() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("char_118_yuki.ab"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let graph = snapshot
        .reference_graph(
            ReferenceGraphBuildOptions::unbounded(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let planner = ExtractionPlanner::new(&snapshot);
    let occurrences = planner
        .bundle_container_occurrences(
            BundleContainerQuery::new("*").unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(occurrences.is_complete());
    assert!(!occurrences.occurrences().is_empty());
    for occurrence in occurrences.occurrences() {
        assert!(!occurrence.asset_path().is_empty());
        assert!(!occurrence.field_path().segments().is_empty());
        assert_eq!(
            occurrence.raw_target().path_id() == 0,
            matches!(occurrence.resolution(), BundleContainerResolution::Null)
        );
        let exact_facts = graph
            .facts()
            .iter()
            .filter(|fact| {
                graph.address(fact.source()).ok() == Some(occurrence.owner())
                    && fact.field_path() == occurrence.field_path()
                    && matches!(
                        fact.raw_target(),
                        RawReferenceTarget::Binary {
                            file_id,
                            path_id,
                            ..
                        } if *file_id == occurrence.raw_target().file_id()
                            && *path_id == occurrence.raw_target().path_id()
                    )
            })
            .count();
        assert_eq!(exact_facts, 1);
    }
    let canonical = occurrences.canonical_json().unwrap();
    let decoded = unity_asset::extraction::BundleContainerResult::read_json(
        canonical.as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    assert_eq!(decoded, occurrences);
    let (container_pattern, address) = occurrences
        .occurrences()
        .iter()
        .find_map(|occurrence| {
            occurrence
                .resolution()
                .resolved()
                .map(|address| (occurrence.asset_path().to_owned(), address.clone()))
        })
        .expect("the fixture AssetBundle must expose at least one container entry");
    let WorkspaceLookup::Resolved(handle) = snapshot
        .resolve_object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("the container-selected fixture object must resolve");
    };

    let container_plan = planner
        .plan(
            ExtractionRequest::bundle_container(
                container_pattern,
                ExtractionRepresentationPolicy::RawOnly,
            )
            .unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let handle_plan = planner
        .plan_handles(
            &[handle],
            ExtractionRepresentationPolicy::RawOnly,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(container_plan.artifacts().len(), 1);
    assert_eq!(handle_plan.artifacts().len(), 1);
    assert_eq!(container_plan.artifacts()[0].address(), &address);
    assert_eq!(handle_plan.artifacts()[0].address(), &address);

    let directory = tempfile::tempdir().unwrap();
    let container_report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &container_plan,
            &directory.path().join("container"),
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let handle_report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &handle_plan,
            &directory.path().join("handle"),
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let container_artifact = &container_report.manifest().artifacts()[0];
    let handle_artifact = &handle_report.manifest().artifacts()[0];
    assert_eq!(
        container_artifact.status(),
        ExtractionArtifactStatus::Written
    );
    assert_eq!(handle_artifact.status(), ExtractionArtifactStatus::Written);
    assert_eq!(container_artifact.length(), handle_artifact.length());
    assert_eq!(container_artifact.digest(), handle_artifact.digest());
}

#[cfg(feature = "decode")]
#[test]
fn execution_rejects_a_serialized_raw_downgrade_of_decodable_media() {
    let directory = tempfile::tempdir().unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("xinzexi_2_n_tex"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let request = ExtractionRequest::all(ExtractionRepresentationPolicy::PreferDecoded)
        .with_filter(
            unity_asset::extraction::ExtractionFilter::new(
                [class_ids::TEXTURE_2D],
                None,
                None,
                None,
            )
            .unwrap(),
        );
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(request, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(plan.artifacts().len(), 1);

    let mut wire: serde_json::Value =
        serde_json::from_slice(&plan.canonical_json().unwrap()).unwrap();
    let owner_locator =
        serde_json::to_value(plan.artifacts()[0].address().source_locator()).unwrap();
    let decoded_path = wire["artifacts"][0]["preferred_path"].as_str().unwrap();
    let raw_path = format!(
        "{}.bin",
        decoded_path
            .strip_suffix(".png")
            .expect("texture plan uses the canonical PNG suffix")
    );
    let address = wire["artifacts"][0]["address"].clone();
    wire["artifacts"][0]["preferred_kind"] = serde_json::json!("binary_raw");
    wire["artifacts"][0]["preferred_path"] = serde_json::json!(raw_path);
    wire["artifacts"][0]["preferred_content"] = serde_json::json!({
        "kind": "raw_binary",
    });
    wire["artifacts"][0]["representation_semantics"] = serde_json::json!({
        "kind": "raw_binary",
        "bytes": "workspace_object_raw_bytes_v1",
    });
    wire["artifacts"][0]["fallback"] = serde_json::Value::Null;
    wire["artifacts"][0]["diagnostics"] = serde_json::json!([{
        "code": "feature_unavailable",
        "address": address,
    }]);
    wire["sources"]
        .as_array_mut()
        .unwrap()
        .retain(|source| source["locator"] == owner_locator);
    let tampered = ExtractionPlan::read_json(
        serde_json::to_vec(&wire).unwrap().as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    let output = directory.path().join("raw-downgrade-output");
    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &tampered,
            &output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractionExecutionError::PlanVerification(source)
            if matches!(
                source.as_ref(),
                ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::SourceExpectations,
                }
            )
    ));
    assert!(!output.exists());
}

#[cfg(feature = "decode")]
#[test]
fn streamed_media_execution_rejects_another_loaded_sidecar() {
    let directory = tempfile::tempdir().unwrap();
    let alternate_path = directory.path().join("unrelated.resS");
    fs::write(&alternate_path, b"another loaded sidecar").unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("char_118_yuki.ab"), &mut AssetLoadBudget::default())
        .unwrap();
    let alternate_id = workspace
        .load_path(&alternate_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let WorkspaceLookup::Resolved(alternate) = snapshot
        .source(alternate_id, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("alternate sidecar must remain loaded");
    };
    let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
        .with_filter(
            unity_asset::extraction::ExtractionFilter::new(
                [class_ids::AUDIO_CLIP],
                None,
                None,
                Some(1),
            )
            .unwrap(),
        );
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(request, &mut AssetLoadBudget::default())
        .unwrap();
    let mut wire: serde_json::Value =
        serde_json::from_slice(&plan.canonical_json().unwrap()).unwrap();
    let streamed_index = wire["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .position(|artifact| artifact["preferred_content"]["stream"].is_object())
        .expect("fixture must contain a streamed AudioClip");
    let planned_source =
        wire["artifacts"][streamed_index]["preferred_content"]["stream"]["source"].clone();
    let source_index = wire["sources"]
        .as_array()
        .unwrap()
        .iter()
        .position(|source| source == &planned_source)
        .expect("streamed sidecar must be a global plan precondition");
    let mut missing_precondition = wire.clone();
    missing_precondition["sources"]
        .as_array_mut()
        .unwrap()
        .remove(source_index);
    let error = ExtractionPlan::read_json(
        serde_json::to_vec(&missing_precondition)
            .unwrap()
            .as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("has no expected fingerprint"));

    wire["artifacts"][streamed_index]["preferred_content"]["stream"]["source"]["locator"] =
        serde_json::to_value(alternate.locator()).unwrap();
    wire["artifacts"][streamed_index]["preferred_content"]["stream"]["source"]["fingerprint"] =
        serde_json::to_value(alternate.fingerprint()).unwrap();
    wire["sources"][source_index]["locator"] = serde_json::to_value(alternate.locator()).unwrap();
    wire["sources"][source_index]["fingerprint"] =
        serde_json::to_value(alternate.fingerprint()).unwrap();
    let plan = ExtractionPlan::read_json(
        serde_json::to_vec(&wire).unwrap().as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    let output = directory.path().join("wrong-sidecar-output");
    let error = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ExtractionExecutionError::PlanVerification(source)
            if matches!(
                source.as_ref(),
                ExtractionPlanError::PlanDerivationMismatch {
                    kind: ExtractionPlanMismatchKind::SourceExpectations,
                }
            )
    ));
    assert!(!output.exists());
}

#[cfg(feature = "decode")]
#[test]
fn unsupported_binary_classes_are_reported_without_silent_raw_downgrade() {
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_path(sample("char_118_yuki.ab"), &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let planner = ExtractionPlanner::new(&snapshot);
    let request = || ExtractionRequest::all(ExtractionRepresentationPolicy::PreferDecoded);
    let cold_plan = planner
        .plan(request(), &mut AssetLoadBudget::default())
        .unwrap();
    let mut measured = AssetLoadBudget::default();
    let preferred = planner.plan(request(), &mut measured).unwrap();
    assert_eq!(preferred, cold_plan);
    let artifact = preferred
        .artifacts()
        .iter()
        .find(|artifact| {
            artifact
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.code() == ExtractionDiagnosticCode::UnsupportedClass)
        })
        .expect("fixture must contain a binary class without a media decoder");
    let usage = measured.usage();
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
    assert_eq!(planner.plan(request(), &mut exact).unwrap(), preferred);
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

    assert_eq!(
        artifact.preferred_kind(),
        unity_asset::extraction::ExtractionArtifactKind::BinaryRaw
    );
    let artifact_index = preferred
        .artifacts()
        .iter()
        .position(|candidate| candidate.address() == artifact.address())
        .unwrap();
    let mut diagnostic_wire: serde_json::Value =
        serde_json::from_slice(&preferred.canonical_json().unwrap()).unwrap();
    diagnostic_wire["artifacts"][artifact_index]["diagnostics"] = serde_json::json!([]);
    let error = ExtractionPlan::read_json(
        serde_json::to_vec(&diagnostic_wire).unwrap().as_slice(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires a deterministic planning diagnostic")
    );

    let output = tempfile::tempdir().unwrap();
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &preferred,
            output.path(),
            ExtractionRunOptions::new(options(1, ExistingOutputPolicy::Error)),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let receipt = report
        .manifest()
        .artifacts()
        .iter()
        .find(|candidate| candidate.address() == artifact.address())
        .expect("planned unsupported-class artifact must have an execution receipt");
    assert!(
        receipt
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code() == ExtractionDiagnosticCode::UnsupportedClass)
    );

    let error = planner
        .plan(
            ExtractionRequest::addresses(
                [artifact.address().clone()],
                ExtractionRepresentationPolicy::RequireDecoded,
            )
            .unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractionPlanError::RequiredDecodedUnavailable {
            reason: ExtractionDiagnosticCode::UnsupportedClass,
            ..
        }
    ));
}
